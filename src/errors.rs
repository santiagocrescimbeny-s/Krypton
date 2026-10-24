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

    /// Real-time stream monitor failure (Geyser connection, account decoding).
    #[error("monitor error: {0}")]
    Monitor(#[from] MonitorError),

    /// Exit manager failure (plan validation, sell quoting, order dispatch).
    #[error("exit error: {0}")]
    Exit(#[from] ExitError),

    /// Logging subscriber setup failure.
    #[error("logging error: {0}")]
    Logging(#[from] LoggingError),

    /// Dry-run simulation harness failure.
    #[error("simulation error: {0}")]
    Simulation(#[from] SimulationError),

    /// Live-trading barrier, spending guard or live execution failure.
    #[error("live trading error: {0}")]
    Live(#[from] LiveError),

    /// Yellowstone adapter configuration failure.
    #[error("yellowstone error: {0}")]
    Yellowstone(#[from] YellowstoneError),

    /// Command-line failure.
    #[error("command line error: {0}")]
    Cli(#[from] CliError),

    /// Production bridge failure (barrier, blockhash source, services).
    #[error("bridge error: {0}")]
    Bridge(#[from] BridgeError),

    /// Jito tip-floor feed failure.
    #[error("tip feed error: {0}")]
    TipFeed(#[from] TipFeedError),

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

    /// `true` when a failed `sendBundle` leaves open whether the Block Engine
    /// accepted the bundle: a timeout, or a connection-level error after the
    /// connection was established. A refusal to connect, an HTTP status or a
    /// JSON-RPC error means the bundle was not accepted.
    ///
    /// Such a bundle may still land, so its sell must not be repeated blindly.
    #[must_use]
    pub fn may_have_been_accepted(&self) -> bool {
        match self {
            Self::Timeout => true,
            Self::Transport(source) => {
                !(source.is_builder()
                    || source.is_connect()
                    || source.is_redirect()
                    || source.is_status()
                    || source.is_decode())
            }
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

    /// A sell of zero tokens was requested.
    #[error("the sell amount resolves to zero tokens")]
    ZeroSellAmount,

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

/// Coarse classification of a stream transport failure.
///
/// Transport adapters (for example a Yellowstone gRPC client) map their native
/// status codes onto these kinds, so the error type stays free of remote text
/// (which could echo endpoints or tokens) and is always safe to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    /// The endpoint could not be reached (DNS, TCP, refused, reset).
    Unreachable,
    /// TLS negotiation failed.
    Tls,
    /// The endpoint rejected the credentials (`x-token`). Never retried.
    Unauthenticated,
    /// The endpoint refused the subscription request as malformed. Never retried.
    InvalidRequest,
    /// The endpoint shed load (`RESOURCE_EXHAUSTED`, `UNAVAILABLE`, rate limit).
    Overloaded,
    /// The peer violated the stream protocol or sent undecodable data.
    Protocol,
    /// Any other failure.
    Other,
}

impl std::fmt::Display for StreamErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Unreachable => "endpoint unreachable",
            Self::Tls => "TLS failure",
            Self::Unauthenticated => "authentication rejected",
            Self::InvalidRequest => "subscription rejected",
            Self::Overloaded => "endpoint overloaded",
            Self::Protocol => "protocol violation",
            Self::Other => "other failure",
        };
        f.write_str(text)
    }
}

/// Failures of the real-time stream monitor (Phase 5).
///
/// Variants carry only static labels and numbers, so they are always safe to log.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MonitorError {
    /// The transport failed to connect or the stream broke.
    #[error("stream connection failure: {kind}")]
    Connection {
        /// Classification of the failure.
        kind: StreamErrorKind,
    },

    /// The server closed the stream cleanly.
    #[error("the stream was closed by the server")]
    StreamClosed,

    /// No update (not even a heartbeat) arrived within the idle deadline.
    #[error("no stream activity for {millis} ms")]
    IdleTimeout {
        /// Deadline that was exceeded, in milliseconds.
        millis: u64,
    },

    /// Establishing the connection took longer than the allowed deadline.
    #[error("connecting took longer than {millis} ms")]
    ConnectTimeout {
        /// Deadline that was exceeded, in milliseconds.
        millis: u64,
    },

    /// The reconnection budget is exhausted.
    #[error("gave up after {attempts} consecutive failed connection attempts")]
    ReconnectExhausted {
        /// Consecutive failures before giving up.
        attempts: u32,
    },

    /// An account payload is shorter than the layout requires.
    #[error("account data is {actual} bytes, at least {min} required")]
    AccountTooShort {
        /// Bytes received.
        actual: usize,
        /// Minimum bytes the layout needs.
        min: usize,
    },

    /// The account does not start with the expected Anchor discriminator.
    #[error("unexpected account discriminator")]
    UnexpectedDiscriminator,

    /// A decoded field holds a value no valid account can have.
    #[error("invalid account data: {reason}")]
    InvalidAccountData {
        /// Static, value-free explanation.
        reason: &'static str,
    },

    /// Monitor settings are inconsistent.
    #[error("invalid monitor configuration: {reason}")]
    InvalidConfig {
        /// Static explanation.
        reason: &'static str,
    },

    /// The receiving side of an internal channel is gone.
    #[error("the `{channel}` channel is closed")]
    ChannelClosed {
        /// Logical name of the channel.
        channel: &'static str,
    },
}

impl MonitorError {
    /// Whether reconnecting can plausibly fix this failure.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Connection { kind } => !matches!(
                kind,
                StreamErrorKind::Unauthenticated | StreamErrorKind::InvalidRequest
            ),
            Self::StreamClosed | Self::IdleTimeout { .. } | Self::ConnectTimeout { .. } => true,
            _ => false,
        }
    }
}

/// Failures of the exit manager (Phase 5): plan validation, sell quoting and
/// order dispatch.
///
/// Variants carry only static labels, numbers and the (log-safe) Jito and builder
/// errors. `PartialEq` is not derived because [`JitoError`] wraps transport errors.
#[derive(Debug, Error)]
pub enum ExitError {
    /// The exit plan is inconsistent (empty, out of window, fractions above 100 %).
    #[error("invalid exit plan: {reason}")]
    InvalidPlan {
        /// Static explanation.
        reason: &'static str,
    },

    /// An exit setting is out of range.
    #[error("invalid exit configuration: {reason}")]
    InvalidConfig {
        /// Static explanation.
        reason: &'static str,
    },

    /// A tranche came due before any bonding curve state was observed.
    #[error("no bonding curve state observed yet; cannot quote a sell")]
    NoCurveState,

    /// The curve already completed (graduated); `sell_v3` is no longer possible.
    #[error("the bonding curve is complete; selling on the curve is no longer possible")]
    CurveComplete,

    /// Cashback coins reject `sell_v3` on chain (`CashbackCoinNotSupported`).
    #[error("cashback coins are not supported by sell_v3")]
    CashbackCoinUnsupported,

    /// A tranche came due too late to be issued inside the execution window.
    #[error("tranche {tranche} missed the execution window")]
    WindowMissed {
        /// Zero-based index of the tranche in the plan.
        tranche: u8,
    },

    /// Issuing another order would exceed the total fee budget.
    #[error("order cost of {cost} lamports would exceed the remaining budget of {remaining}")]
    BudgetExceeded {
        /// Worst-case cost of the order, in lamports.
        cost: u64,
        /// Budget left, in lamports.
        remaining: u64,
    },

    /// The order channel is closed (executor gone).
    #[error("the sell order channel is closed")]
    OrderChannelClosed,

    /// The order channel is full (executor stalled).
    #[error("the sell order channel is full")]
    OrderChannelFull,

    /// Checked arithmetic overflowed.
    #[error("arithmetic overflow while computing {context}")]
    ArithmeticOverflow {
        /// What was being computed.
        context: &'static str,
    },

    /// The cached blockhash is older than the allowed age: the refresher has
    /// stalled, and a transaction signed with it would expire silently.
    #[error("the cached blockhash is too old to sign with; the refresher may have stalled")]
    StaleBlockhash,

    /// Sell quoting or transaction assembly failed.
    #[error("sell builder error: {0}")]
    Builder(#[from] BuilderError),

    /// Bundle submission failed.
    #[error("Jito error: {0}")]
    Jito(#[from] JitoError),

    /// A live spending guard refused the order (per-transaction cap, daily
    /// drawdown or the halt latch). Carries no amounts.
    #[error("a spending guard refused the order")]
    GuardRejected(#[from] GuardError),
}

/// Failures of the dry-run simulation harness (Phase 6).
///
/// Variants carry static labels and numbers only, never keys or amounts.
#[derive(Debug, Error)]
pub enum SimulationError {
    /// The harness only runs with `DRY_RUN=true`: it refuses a live configuration
    /// rather than risk being mistaken for the real pipeline.
    #[error("the simulation harness refuses to run unless DRY_RUN=true")]
    LiveModeRefused,

    /// The scenario is inconsistent.
    #[error("invalid simulation scenario: {reason}")]
    InvalidScenario {
        /// Static explanation.
        reason: &'static str,
    },

    /// A shared simulation structure was poisoned by a panicking task.
    #[error("the simulated market state is unavailable")]
    StatePoisoned,

    /// A simulation task ended abnormally.
    #[error("the `{task}` simulation task failed")]
    TaskFailed {
        /// Which task.
        task: &'static str,
    },

    /// Curve arithmetic, launch assembly or sell assembly failed.
    #[error("builder error: {0}")]
    Builder(#[from] BuilderError),

    /// The exit engine or manager failed.
    #[error("exit error: {0}")]
    Exit(#[from] ExitError),

    /// The stream monitor failed.
    #[error("monitor error: {0}")]
    Monitor(#[from] MonitorError),

    /// The simulated tip could not be built.
    #[error("tip error: {0}")]
    Jito(#[from] JitoError),

    /// The simulation wallet could not be created.
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}

/// Refusals of the live spending guards (Phase 7).
///
/// Variants carry static labels only: no amounts, limits or balances, so they
/// are safe to log.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum GuardError {
    /// The guard limits are inconsistent or out of range.
    #[error("invalid spending limits: {reason}")]
    InvalidLimits {
        /// Static explanation.
        reason: &'static str,
    },

    /// One transaction would spend more than the per-transaction cap.
    #[error("the transaction exceeds the per-transaction spending cap")]
    PerTransactionCap,

    /// The order would take the day's net spend past the drawdown limit.
    #[error("the order would exceed the daily drawdown limit")]
    DailyDrawdown,

    /// The drawdown tripwire fired earlier; new entries stay blocked until an
    /// explicit re-arm.
    #[error("new entries are halted after the daily drawdown tripwire")]
    Halted,

    /// The requested slippage tolerance exceeds the 10 % hard cap.
    #[error("the slippage tolerance exceeds the 10% hard cap")]
    SlippageTooHigh,

    /// The guard state was poisoned by a panicking task.
    #[error("the spending guard state is unavailable")]
    StatePoisoned,

    /// The persisted accounting cannot be kept up to date (the state file
    /// cannot be written). New entries are refused until a write succeeds;
    /// exits are never blocked.
    #[error("new entries are refused: the spending state cannot be persisted")]
    PersistenceFailed,

    /// The persisted accounting belongs to a day that has not happened yet: the
    /// system clock went backwards, or the file is not ours.
    #[error("the persisted spending state is dated in the future")]
    PersistedStateInFuture,
}

/// Failures of the persisted spending-guard state file (audit finding #35).
///
/// Static labels only: the path and the contents are never echoed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StateFileError {
    /// The state file does not exist. Create it once, on purpose, with
    /// `krypton init-state`.
    #[error("the guard state file does not exist (run `krypton init-state` once)")]
    Missing,

    /// The state file exists and must not be overwritten by `init-state`.
    #[error("the guard state file already exists")]
    AlreadyExists,

    /// Another process holds the state file (a live session, or `check`).
    #[error("the guard state file is in use by another process")]
    Locked,

    /// The file could not be read.
    #[error("the guard state file cannot be read ({reason})")]
    Unreadable {
        /// Static explanation.
        reason: &'static str,
    },

    /// The file has unsafe permissions or is not a regular file.
    #[error("the guard state file is not safe to trust ({reason})")]
    Insecure {
        /// Static explanation.
        reason: &'static str,
    },

    /// The contents are not a valid, intact state record.
    #[error("the guard state file is corrupted ({reason})")]
    Corrupt {
        /// Static explanation.
        reason: &'static str,
    },

    /// The file could not be written atomically.
    #[error("the guard state file cannot be written ({reason})")]
    Write {
        /// Static explanation.
        reason: &'static str,
    },
}

/// Failures of the dry-to-live barrier and of live execution (Phase 7).
///
/// Variants carry static labels only (no amounts, paths or keys).
#[derive(Debug, Error)]
pub enum LiveError {
    /// `DRY_RUN` is still `true`.
    #[error("live trading refused: DRY_RUN must be explicitly set to false")]
    DryRunEnabled,

    /// `LIVE_TRADING` is not `true`.
    #[error("live trading refused: LIVE_TRADING=true is required")]
    NotArmed,

    /// `LIVE_CONFIRM` does not carry the confirmation phrase.
    #[error("live trading refused: LIVE_CONFIRM must carry the confirmation phrase")]
    ConfirmationMissing,

    /// A mandatory capital limit is not configured.
    #[error("live trading refused: {name} is required")]
    LimitMissing {
        /// The environment variable.
        name: &'static str,
    },

    /// `LIVE_KEYPAIR_PATH` is not set.
    #[error("live trading refused: LIVE_KEYPAIR_PATH is required")]
    KeypairPathMissing,

    /// The keypair file failed a safety check.
    #[error("live trading refused: keypair file rejected ({reason})")]
    KeypairFile {
        /// Static explanation.
        reason: &'static str,
    },

    /// The keypair file is not the configured wallet.
    #[error("live trading refused: the keypair file does not match the configured wallet")]
    KeypairMismatch,

    /// The Block Engine client is in dry-run mode.
    #[error("live trading refused: the Block Engine client is in dry-run mode")]
    EngineIsDryRun,

    /// The tip policy parameters are inconsistent.
    #[error("invalid tip policy: {reason}")]
    InvalidTipPolicy {
        /// Static explanation.
        reason: &'static str,
    },

    /// The tip floor feed returned unusable data.
    #[error("tip floor feed error: {reason}")]
    TipFeed {
        /// Static explanation.
        reason: &'static str,
    },

    /// `GUARD_STATE_PATH` is not set.
    #[error("live trading refused: GUARD_STATE_PATH is required")]
    GuardStatePathMissing,

    /// The persisted spending state is missing, unreadable, insecure, corrupted
    /// or in the future. Live trading never starts from a state it cannot trust.
    #[error("live trading refused: {0}")]
    GuardState(#[from] StateFileError),

    /// A shared live structure was poisoned by a panicking task.
    #[error("live execution state is unavailable")]
    StatePoisoned,

    /// The system clock is before the Unix epoch.
    #[error("the system clock is unavailable")]
    ClockUnavailable,

    /// A live task ended abnormally.
    #[error("the `{task}` live task failed")]
    TaskFailed {
        /// Which task.
        task: &'static str,
    },

    /// A spending guard refused the order.
    #[error("spending guard: {0}")]
    Guard(#[from] GuardError),

    /// Transaction construction failed.
    #[error("builder error: {0}")]
    Builder(#[from] BuilderError),

    /// The Block Engine failed.
    #[error("Jito error: {0}")]
    Jito(#[from] JitoError),

    /// The exit pipeline failed.
    #[error("exit error: {0}")]
    Exit(#[from] ExitError),

    /// The stream monitor failed.
    #[error("monitor error: {0}")]
    Monitor(#[from] MonitorError),
}

/// Failures while installing the `tracing` subscriber.
///
/// Carries only static text, so it is always safe to log or print.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LoggingError {
    /// The filter directive (`RUST_LOG`) could not be parsed.
    #[error("invalid log filter directive")]
    InvalidFilter,

    /// A global subscriber is already installed.
    #[error("a global tracing subscriber is already installed")]
    AlreadyInstalled,
}

impl ExitError {
    /// A short, stable, value-free name for the error, safe for logs: unlike
    /// the `Display` text it never carries amounts, budgets or balances.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::InvalidPlan { .. } => "invalid_plan",
            Self::InvalidConfig { .. } => "invalid_config",
            Self::NoCurveState => "no_curve_state",
            Self::CurveComplete => "curve_complete",
            Self::CashbackCoinUnsupported => "cashback_unsupported",
            Self::WindowMissed { .. } => "window_missed",
            Self::BudgetExceeded { .. } => "budget_exceeded",
            Self::OrderChannelClosed => "order_channel_closed",
            Self::OrderChannelFull => "order_channel_full",
            Self::ArithmeticOverflow { .. } => "arithmetic_overflow",
            Self::StaleBlockhash => "stale_blockhash",
            Self::Builder(_) => "builder_error",
            Self::Jito(_) => "jito_error",
            Self::GuardRejected(_) => "guard_rejected",
        }
    }
}

/// Invalid Yellowstone adapter configuration (Phase 8).
///
/// Static text only: the endpoint (which may embed an access token in its path)
/// is never echoed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum YellowstoneError {
    /// The endpoint URL is unusable.
    #[error("invalid yellowstone endpoint: {reason}")]
    InvalidEndpoint {
        /// Static explanation.
        reason: &'static str,
    },

    /// The `x-token` is unusable.
    #[error("invalid yellowstone token: {reason}")]
    InvalidToken {
        /// Static explanation.
        reason: &'static str,
    },
}

/// Failures of the Jito tip-floor feed (Phase 8).
///
/// Variants carry only static labels and numbers: remote text (which could echo
/// URLs or tokens) is never copied into an error.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TipFeedError {
    /// The feed settings are inconsistent.
    #[error("invalid tip feed configuration: {reason}")]
    InvalidConfig {
        /// Static explanation.
        reason: &'static str,
    },

    /// The feed could not be reached.
    #[error("the tip feed is unreachable")]
    Unreachable,

    /// The feed did not answer within the deadline.
    #[error("the tip feed timed out")]
    Timeout,

    /// The feed answered with a non-success HTTP status.
    #[error("the tip feed answered with HTTP status {status}")]
    HttpStatus {
        /// The status code.
        status: u16,
    },

    /// The response body exceeds the size cap.
    #[error("the tip feed response is too large")]
    ResponseTooLarge,

    /// The response is not a valid tip-floor document.
    #[error("malformed tip feed response: {reason}")]
    Malformed {
        /// Static explanation.
        reason: &'static str,
    },

    /// The shared tip book was poisoned by a panicking task.
    #[error("the tip book is unavailable")]
    BookUnavailable,
}

impl TipFeedError {
    /// A short, stable, value-free name for the error, safe for logs.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::InvalidConfig { .. } => "invalid_config",
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
            Self::HttpStatus { .. } => "http_status",
            Self::ResponseTooLarge => "response_too_large",
            Self::Malformed { .. } => "malformed",
            Self::BookUnavailable => "book_unavailable",
        }
    }
}

/// Command-line failures.
///
/// Carries only static text: arguments are never echoed (a mistyped value may
/// be a secret).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CliError {
    /// No command was given.
    #[error("no command given (try `help`)")]
    NoCommand,

    /// The command is not one of `check`, `exit`, `help`.
    #[error("unknown command (try `help`)")]
    UnknownCommand,

    /// A flag that does not exist (or does not belong to the command).
    #[error("unknown argument (try `help`)")]
    UnknownArgument,

    /// More than one command.
    #[error("only one command may be given")]
    ExtraCommand,

    /// A flag that needs a value has none.
    #[error("{flag} needs a value")]
    MissingValue {
        /// The flag.
        flag: &'static str,
    },

    /// A mandatory flag is absent.
    #[error("{flag} is required")]
    MissingFlag {
        /// The flag.
        flag: &'static str,
    },

    /// A flag was given twice.
    #[error("{flag} was given more than once")]
    DuplicateFlag {
        /// The flag.
        flag: &'static str,
    },

    /// A flag has an unusable value.
    #[error("{flag} is invalid: {reason}")]
    InvalidValue {
        /// The flag.
        flag: &'static str,
        /// Static explanation.
        reason: &'static str,
    },
}

/// Failures of the production bridge (`app`, `network::rpc`, the services).
///
/// Carries only static text and status codes: never URLs, keys or amounts.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BridgeError {
    /// `--live` was given without the exact `--confirm-live` phrase.
    #[error("live trading refused: --confirm-live must carry the confirmation phrase")]
    LiveConfirmationMissing,

    /// The recent blockhash could not be fetched.
    #[error("the recent blockhash is unavailable")]
    BlockhashUnavailable,

    /// The RPC node answered with a non-success HTTP status.
    #[error("the RPC node answered with HTTP status {status}")]
    RpcStatus {
        /// The status code.
        status: u16,
    },

    /// The RPC answer is not a valid `getLatestBlockhash` response.
    #[error("the RPC answer is malformed")]
    RpcMalformed,

    /// The RPC answer exceeds the size cap.
    #[error("the RPC answer is too large")]
    RpcResponseTooLarge,

    /// The RPC client could not be built.
    #[error("invalid RPC client configuration: {reason}")]
    RpcConfig {
        /// Static explanation.
        reason: &'static str,
    },

    /// A background service ended abnormally.
    #[error("the `{service}` service failed")]
    ServiceFailed {
        /// Which service.
        service: &'static str,
    },

    /// The binary was built without a Yellowstone gRPC link.
    #[error("this build has no Yellowstone gRPC link (see docs/yellowstone-adapter.md)")]
    GrpcNotCompiled,

    /// The Yellowstone endpoint is not configured.
    #[error("YELLOWSTONE_ENDPOINT is required for this command")]
    YellowstoneNotConfigured,

    /// The async runtime could not be started.
    #[error("the async runtime could not be started")]
    Runtime,
}

impl BridgeError {
    /// A short, stable, value-free name for logs.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::LiveConfirmationMissing => "live_confirmation_missing",
            Self::BlockhashUnavailable => "blockhash_unavailable",
            Self::RpcStatus { .. } => "rpc_status",
            Self::RpcMalformed => "rpc_malformed",
            Self::RpcResponseTooLarge => "rpc_response_too_large",
            Self::RpcConfig { .. } => "rpc_config",
            Self::ServiceFailed { .. } => "service_failed",
            Self::GrpcNotCompiled => "grpc_not_compiled",
            Self::YellowstoneNotConfigured => "yellowstone_not_configured",
            Self::Runtime => "runtime",
        }
    }
}
