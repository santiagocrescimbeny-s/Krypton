//! Jito Block Engine: tip instruction, launch-bundle assembly and a
//! non-blocking JSON-RPC client.
//!
//! # Transport
//! The client speaks the Block Engine's JSON-RPC API (`POST {engine}/api/v1/bundles`)
//! over HTTPS with `reqwest` + rustls; the gRPC searcher interface is not used
//! (it would add `tonic`/`prost` and generated protobufs to the dependency tree
//! for no gain on this path). Bundles are sent as `base64`, the encoding Jito
//! recommends (`base58` is deprecated).
//!
//! # Safety properties
//! - **Dry-run is enforced here**, at the lowest layer: with `DRY_RUN=true`
//!   [`JitoClient::send_bundle`] returns [`JitoError::DryRun`] before any I/O.
//! - **Tips are capped.** A [`JitoTip`] can only be built within
//!   `MIN_JITO_TIP_LAMPORTS..=min(MAX_JITO_TIP_LAMPORTS, ABSOLUTE_MAX_JITO_TIP_LAMPORTS)`,
//!   and [`assemble_launch_transaction`] adds the tip to the worst-case balance
//!   check ([`LaunchBuilder::ensure_funded`]) before anything is signed. The
//!   priority fee keeps its own cap (finding #19), enforced by [`LaunchBuilder`].
//! - **`sendBundle` is never retried** automatically: after an ambiguous failure
//!   (timeout) the caller should poll [`JitoClient::inflight_status`]. The
//!   read-only calls are retried with exponential backoff.
//! - Responses are size-capped, redirects are disabled and error text coming
//!   from the server is sanitized.
//! - The tip account is a plain static key: Jito's documentation forbids
//!   loading it from an Address Lookup Table.

use std::fmt;
use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::transaction::VersionedTransaction;
use tracing::{debug, warn};

use crate::builder::alt::LaunchLookupTable;
use crate::builder::constants::{MAX_TRANSACTION_SIZE, SYSTEM_PROGRAM_ID};
use crate::builder::transaction::{FundingSnapshot, LaunchBuilder, MintSigner};
use crate::config::{redact_url, Config, ABSOLUTE_MAX_JITO_TIP_LAMPORTS, MIN_JITO_TIP_LAMPORTS};
use crate::errors::{sanitize_remote_message, JitoError};
use crate::security::SecureWallet;
use crate::storage::metadata::is_loopback_host;

// --- Constants -------------------------------------------------------------------

/// First official tip account; used if a pick ever falls outside the list.
pub const FALLBACK_TIP_ACCOUNT: Pubkey =
    Pubkey::from_str_const("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5");

/// The eight official mainnet tip accounts
/// (<https://docs.jito.wtf/lowlatencytxnsend/>). The engine's `getTipAccounts`
/// is authoritative; this list is the offline fallback.
pub const JITO_TIP_ACCOUNTS: [Pubkey; 8] = [
    FALLBACK_TIP_ACCOUNT,
    Pubkey::from_str_const("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    Pubkey::from_str_const("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    Pubkey::from_str_const("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    Pubkey::from_str_const("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    Pubkey::from_str_const("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    Pubkey::from_str_const("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    Pubkey::from_str_const("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];

/// JSON-RPC path of the bundle methods.
pub const BUNDLES_PATH: &str = "/api/v1/bundles";
/// Maximum transactions per bundle.
pub const MAX_BUNDLE_TRANSACTIONS: usize = 5;
/// Default per-request deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default retries (read-only calls) after the first attempt.
pub const DEFAULT_MAX_RETRIES: u32 = 2;
/// Upper bound accepted for the retry count.
pub const MAX_RETRIES_CAP: u32 = 5;
/// Default base delay of the exponential backoff.
pub const DEFAULT_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
/// Minimum wait after an HTTP 429 (the public engine allows 1 request/second).
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const USER_AGENT: &str = concat!("krypton/", env!("CARGO_PKG_VERSION"));
/// Index of `SystemInstruction::Transfer`.
const SYSTEM_TRANSFER: u32 = 2;
/// Length of a bundle id (hex SHA-256).
const BUNDLE_ID_LEN: usize = 64;

// --- Tip -------------------------------------------------------------------------

/// Picks a tip account deterministically from the mint, spreading load across
/// the eight official accounts (Jito suggests a random pick; any of them is valid).
#[must_use]
pub fn tip_account_for(mint: &Pubkey) -> Pubkey {
    let first = mint.to_bytes().first().copied().unwrap_or(0);
    let index = usize::from(first) % JITO_TIP_ACCOUNTS.len();
    JITO_TIP_ACCOUNTS
        .get(index)
        .copied()
        .unwrap_or(FALLBACK_TIP_ACCOUNT)
}

/// `SystemProgram::Transfer` of `lamports` from `payer` to `tip_account`.
///
/// Both accounts are static keys (never loaded from a lookup table). The amount
/// is checked against the Block Engine minimum and the absolute ceiling; the
/// configured cap is enforced by [`JitoTip`].
///
/// # Errors
/// [`JitoError::TipTooLow`] / [`JitoError::TipTooHigh`].
pub fn build_jito_tip_instruction(
    payer: &Pubkey,
    tip_account: &Pubkey,
    lamports: u64,
) -> Result<Instruction, JitoError> {
    if lamports < MIN_JITO_TIP_LAMPORTS {
        return Err(JitoError::TipTooLow {
            lamports,
            min: MIN_JITO_TIP_LAMPORTS,
        });
    }
    if lamports > ABSOLUTE_MAX_JITO_TIP_LAMPORTS {
        return Err(JitoError::TipTooHigh {
            lamports,
            max: ABSOLUTE_MAX_JITO_TIP_LAMPORTS,
        });
    }
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&SYSTEM_TRANSFER.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Ok(Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*tip_account, false),
        ],
        data,
    })
}

/// A validated tip: an account and an amount within the configured limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitoTip {
    account: Pubkey,
    lamports: u64,
}

impl JitoTip {
    /// Validates `lamports` against the Block Engine minimum and
    /// `min(max_lamports, ABSOLUTE_MAX_JITO_TIP_LAMPORTS)`.
    ///
    /// # Errors
    /// [`JitoError::TipTooLow`] / [`JitoError::TipTooHigh`].
    pub fn new(account: Pubkey, lamports: u64, max_lamports: u64) -> Result<Self, JitoError> {
        let cap = max_lamports.min(ABSOLUTE_MAX_JITO_TIP_LAMPORTS);
        if lamports < MIN_JITO_TIP_LAMPORTS {
            return Err(JitoError::TipTooLow {
                lamports,
                min: MIN_JITO_TIP_LAMPORTS,
            });
        }
        if lamports > cap {
            return Err(JitoError::TipTooHigh { lamports, max: cap });
        }
        Ok(Self { account, lamports })
    }

    /// The tip configured in `JITO_TIP_LAMPORTS`, capped by `MAX_JITO_TIP_LAMPORTS`.
    ///
    /// # Errors
    /// See [`JitoTip::new`].
    pub fn from_config(config: &Config, account: Pubkey) -> Result<Self, JitoError> {
        Self::new(
            account,
            config.jito_tip_lamports,
            config.max_jito_tip_lamports,
        )
    }

    /// Recipient of the tip.
    #[must_use]
    pub fn account(&self) -> Pubkey {
        self.account
    }

    /// Amount, in lamports.
    #[must_use]
    pub fn lamports(&self) -> u64 {
        self.lamports
    }

    /// The transfer instruction paid by `payer`.
    ///
    /// # Errors
    /// See [`build_jito_tip_instruction`].
    pub fn instruction(&self, payer: &Pubkey) -> Result<Instruction, JitoError> {
        build_jito_tip_instruction(payer, &self.account, self.lamports)
    }
}

// --- Launch assembly ---------------------------------------------------------------

/// Everything [`assemble_launch_transaction`] needs besides the builder and the keys.
#[derive(Debug, Clone, Copy)]
pub struct LaunchBundleInputs<'a> {
    /// Current lamport balance of the payer (`getBalance`).
    pub available_lamports: u64,
    /// Rent of the accounts `Create` allocates (measure it by simulation).
    pub create_rent_lamports: u64,
    /// The tip appended after `Buy`.
    pub tip: &'a JitoTip,
    /// Freshly fetched blockhash: the last datum injected before signing.
    pub recent_blockhash: Hash,
    /// The launch's lookup table.
    pub table: &'a LaunchLookupTable,
}

/// Builds the single signed transaction of the launch bundle:
/// `[compute budget, ATA, Create, Buy, tip]` compiled against the launch table.
///
/// The worst-case balance check includes the tip and runs before any message is
/// compiled or key is touched.
///
/// # Errors
/// [`JitoError::Builder`] (balance, size, signing) or the tip errors.
pub fn assemble_launch_transaction(
    builder: &LaunchBuilder,
    inputs: &LaunchBundleInputs<'_>,
    wallet: &SecureWallet,
    mint: &MintSigner,
) -> Result<VersionedTransaction, JitoError> {
    let tip_instruction = inputs.tip.instruction(&builder.payer())?;
    let funding = FundingSnapshot {
        available_lamports: inputs.available_lamports,
        create_rent_lamports: inputs.create_rent_lamports,
        tip_lamports: inputs.tip.lamports(),
    };
    let tables = [inputs.table.to_account()];
    Ok(builder.build_signed_checked(
        &funding,
        inputs.recent_blockhash,
        &tables,
        std::slice::from_ref(&tip_instruction),
        wallet,
        mint,
    )?)
}

// --- Wire encoding -------------------------------------------------------------------

fn push_short_vec_len(out: &mut Vec<u8>, len: usize) {
    let mut remaining = len;
    loop {
        let low = u8::try_from(remaining & 0x7f).unwrap_or(0);
        remaining >>= 7;
        if remaining == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Wire format of a transaction: `short_vec` signature count, the signatures,
/// then the serialized message.
#[must_use]
pub fn serialize_transaction(transaction: &VersionedTransaction) -> Vec<u8> {
    let message = transaction.message.serialize();
    let mut out = Vec::with_capacity(
        message
            .len()
            .saturating_add(transaction.signatures.len().saturating_mul(64))
            .saturating_add(3),
    );
    push_short_vec_len(&mut out, transaction.signatures.len());
    for signature in &transaction.signatures {
        out.extend_from_slice(signature.as_ref());
    }
    out.extend_from_slice(&message);
    out
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_char(sextet: u32) -> char {
    usize::try_from(sextet & 0x3f)
        .ok()
        .and_then(|index| BASE64_ALPHABET.get(index))
        .map_or('A', |byte| char::from(*byte))
}

/// Standard Base64 (RFC 4648, padded). Implemented locally to keep the
/// dependency tree, and `Cargo.lock`, unchanged.
#[must_use]
pub fn encode_base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0);
        let second = chunk.get(1).copied();
        let third = chunk.get(2).copied();
        let block = (u32::from(first) << 16)
            | (u32::from(second.unwrap_or(0)) << 8)
            | u32::from(third.unwrap_or(0));
        out.push(base64_char(block >> 18));
        out.push(base64_char(block >> 12));
        out.push(if second.is_some() {
            base64_char(block >> 6)
        } else {
            '='
        });
        out.push(if third.is_some() {
            base64_char(block)
        } else {
            '='
        });
    }
    out
}

// --- Identifiers and statuses ---------------------------------------------------------

/// A bundle id: the hex SHA-256 of the bundle's transaction signatures.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BundleId(String);

impl BundleId {
    /// Validates the shape (64 hex characters) and normalizes to lower case.
    ///
    /// # Errors
    /// [`JitoError::InvalidResponse`] for any other shape.
    pub fn parse(raw: &str) -> Result<Self, JitoError> {
        let trimmed = raw.trim();
        if trimmed.len() != BUNDLE_ID_LEN || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(JitoError::InvalidResponse("malformed bundle id"));
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BundleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// State of a bundle in the engine's five-minute in-flight window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflightStatus {
    /// The engine has no record of the bundle (never received, or expired).
    NotFound,
    /// Rejected as malformed.
    Invalid,
    /// Waiting for an auction / leader.
    Pending,
    /// Processed but not landed.
    Failed,
    /// Landed on chain in `slot`.
    Landed {
        /// Slot in which the bundle landed.
        slot: u64,
    },
}

// --- JSON-RPC types ----------------------------------------------------------------------

#[derive(Serialize)]
struct RpcRequest<'a, P> {
    jsonrpc: &'static str,
    id: u32,
    method: &'a str,
    params: &'a P,
}

#[derive(Serialize)]
struct EncodingOptions {
    encoding: &'static str,
}

#[derive(Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcErrorBody>,
}

#[derive(Deserialize)]
struct RpcErrorBody {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct InflightResult {
    #[serde(default)]
    value: Option<Vec<InflightEntry>>,
}

#[derive(Deserialize)]
struct InflightEntry {
    #[serde(default)]
    bundle_id: String,
    status: String,
    #[serde(default)]
    landed_slot: Option<u64>,
}

// --- Client --------------------------------------------------------------------------------

/// Non-blocking Block Engine client. Cheap to share behind an `Arc`: the inner
/// `reqwest::Client` pools connections, which keeps TLS warm before a launch.
pub struct JitoClient {
    client: Client,
    url: Url,
    timeout: Duration,
    max_retries: u32,
    retry_base_delay: Duration,
    dry_run: bool,
}

impl fmt::Debug for JitoClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JitoClient")
            .field("engine", &redact_url(&self.url))
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("dry_run", &self.dry_run)
            .finish()
    }
}

/// Validates the engine base URL and derives the bundles endpoint.
///
/// HTTPS is mandatory (plain HTTP only to loopback hosts, for local mocks);
/// credentials, queries and fragments are refused.
fn bundles_endpoint(base: &Url) -> Result<Url, JitoError> {
    let bad = |reason: &str| JitoError::InvalidEndpoint {
        reason: reason.to_owned(),
    };
    let host = base
        .host_str()
        .ok_or_else(|| bad("URL must include a host"))?;
    let scheme_ok = match base.scheme() {
        "https" => true,
        "http" => is_loopback_host(host),
        _ => false,
    };
    if !scheme_ok {
        return Err(bad(
            "scheme must be https (http is only allowed for loopback hosts)",
        ));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(bad("must not embed credentials"));
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(bad("must not contain a query string or fragment"));
    }
    let joined = format!("{}{}", base.as_str().trim_end_matches('/'), BUNDLES_PATH);
    Url::parse(&joined).map_err(|_| bad("cannot derive the bundles URL"))
}

impl JitoClient {
    /// Builds a client with a dedicated rustls HTTP client (no redirects).
    ///
    /// `dry_run` mirrors `DRY_RUN`: when `true`, [`JitoClient::send_bundle`] never
    /// touches the network.
    ///
    /// # Errors
    /// [`JitoError::InvalidEndpoint`] or [`JitoError::ClientBuild`].
    pub fn new(engine_url: &Url, dry_run: bool) -> Result<Self, JitoError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(DEFAULT_TIMEOUT)
            .tcp_nodelay(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| JitoError::ClientBuild(err.without_url()))?;
        Self::with_client(client, engine_url, dry_run)
    }

    /// Same as [`JitoClient::new`] around an existing client (shared pool).
    ///
    /// # Errors
    /// [`JitoError::InvalidEndpoint`].
    pub fn with_client(client: Client, engine_url: &Url, dry_run: bool) -> Result<Self, JitoError> {
        Ok(Self {
            client,
            url: bundles_endpoint(engine_url)?,
            timeout: DEFAULT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_delay: DEFAULT_RETRY_BASE_DELAY,
            dry_run,
        })
    }

    /// Client for `JITO_ENGINE_URL`, honouring `DRY_RUN`.
    ///
    /// # Errors
    /// See [`JitoClient::new`].
    pub fn from_config(config: &Config) -> Result<Self, JitoError> {
        Self::new(&config.jito_engine_url, config.dry_run)
    }

    /// Overrides the per-request deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Overrides the retry policy of the read-only calls (`max_retries` is
    /// clamped to [`MAX_RETRIES_CAP`]).
    #[must_use]
    pub fn with_retries(mut self, max_retries: u32, base_delay: Duration) -> Self {
        self.max_retries = max_retries.min(MAX_RETRIES_CAP);
        self.retry_base_delay = base_delay;
        self
    }

    /// `true` when submissions are disabled.
    #[must_use]
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// `getTipAccounts`: the engine's current tip accounts.
    ///
    /// # Errors
    /// Transport / server errors, or [`JitoError::InvalidResponse`] when the
    /// list is empty or contains a malformed address.
    pub async fn tip_accounts(&self) -> Result<Vec<Pubkey>, JitoError> {
        let params: Vec<String> = Vec::new();
        let raw: Vec<String> = self.call("getTipAccounts", &params, true).await?;
        if raw.is_empty() {
            return Err(JitoError::InvalidResponse("empty tip account list"));
        }
        raw.iter()
            .map(|text| {
                parse_pubkey(text)
                    .ok_or(JitoError::InvalidResponse("malformed tip account address"))
            })
            .collect()
    }

    /// `sendBundle`: submits up to five signed transactions as one bundle.
    ///
    /// Never retried automatically (see the module docs). Returns the bundle id.
    ///
    /// # Errors
    /// [`JitoError::DryRun`] in dry-run mode (before any I/O), bundle shape
    /// errors, or transport / server errors.
    pub async fn send_bundle(
        &self,
        transactions: &[VersionedTransaction],
    ) -> Result<BundleId, JitoError> {
        if self.dry_run {
            return Err(JitoError::DryRun);
        }
        if transactions.is_empty() {
            return Err(JitoError::EmptyBundle);
        }
        if transactions.len() > MAX_BUNDLE_TRANSACTIONS {
            return Err(JitoError::BundleTooLarge {
                count: transactions.len(),
                max: MAX_BUNDLE_TRANSACTIONS,
            });
        }
        let mut encoded = Vec::with_capacity(transactions.len());
        for transaction in transactions {
            let wire = serialize_transaction(transaction);
            if wire.len() > MAX_TRANSACTION_SIZE {
                return Err(JitoError::TransactionTooLarge {
                    size: wire.len(),
                    max: MAX_TRANSACTION_SIZE,
                });
            }
            encoded.push(encode_base64(&wire));
        }
        debug!(transactions = encoded.len(), "submitting bundle");
        let params = (encoded, EncodingOptions { encoding: "base64" });
        let id: String = self.call("sendBundle", &params, false).await?;
        BundleId::parse(&id)
    }

    /// `getInflightBundleStatuses` for one bundle (five-minute look-back).
    ///
    /// # Errors
    /// Transport / server errors, or [`JitoError::InvalidResponse`].
    pub async fn inflight_status(&self, id: &BundleId) -> Result<InflightStatus, JitoError> {
        let params = (vec![id.as_str()],);
        let result: InflightResult = self
            .call("getInflightBundleStatuses", &params, true)
            .await?;
        let Some(entries) = result.value else {
            return Ok(InflightStatus::NotFound);
        };
        let entry = entries
            .iter()
            .find(|entry| entry.bundle_id.eq_ignore_ascii_case(id.as_str()))
            .or_else(|| entries.first());
        let Some(entry) = entry else {
            return Ok(InflightStatus::NotFound);
        };
        match entry.status.as_str() {
            "Invalid" => Ok(InflightStatus::Invalid),
            "Pending" => Ok(InflightStatus::Pending),
            "Failed" => Ok(InflightStatus::Failed),
            "Landed" => Ok(InflightStatus::Landed {
                slot: entry
                    .landed_slot
                    .ok_or(JitoError::InvalidResponse("landed bundle without a slot"))?,
            }),
            _ => Err(JitoError::InvalidResponse("unknown bundle status")),
        }
    }

    /// One JSON-RPC call with the retry policy. `idempotent` calls are retried on
    /// transient errors; the others are attempted exactly once.
    async fn call<P: Serialize, T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &P,
        idempotent: bool,
    ) -> Result<T, JitoError> {
        let max_attempts = if idempotent {
            self.max_retries.saturating_add(1)
        } else {
            1
        };
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            let error = match self.attempt(method, params).await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if !error.is_retryable() {
                return Err(error);
            }
            if attempt >= max_attempts {
                return Err(if attempt > 1 {
                    JitoError::RetriesExhausted {
                        attempts: attempt,
                        source: Box::new(error),
                    }
                } else {
                    error
                });
            }
            warn!(method, attempt, max_attempts, error = %error, "transient Block Engine failure, retrying");
            let pause = if matches!(error, JitoError::RateLimited) {
                self.backoff(attempt).max(RATE_LIMIT_PAUSE)
            } else {
                self.backoff(attempt)
            };
            tokio::time::sleep(pause).await;
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(16);
        self.retry_base_delay
            .saturating_mul(2u32.saturating_pow(exponent))
            .min(MAX_BACKOFF)
    }

    async fn attempt<P: Serialize, T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &P,
    ) -> Result<T, JitoError> {
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };
        let response = self
            .client
            .post(self.url.clone())
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(map_transport_error)?;
        let status = response.status();
        let body = read_capped(response).await?;
        decode_response(status, &body)
    }
}

fn parse_pubkey(text: &str) -> Option<Pubkey> {
    let bytes = bs58::decode(text.trim()).into_vec().ok()?;
    let array = <[u8; 32]>::try_from(bytes.as_slice()).ok()?;
    Some(Pubkey::new_from_array(array))
}

fn map_transport_error(err: reqwest::Error) -> JitoError {
    if err.is_timeout() {
        JitoError::Timeout
    } else {
        JitoError::Transport(err.without_url())
    }
}

/// Buffers a response body, refusing anything above [`MAX_RESPONSE_BYTES`].
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, JitoError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_transport_error)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(JitoError::InvalidResponse(
                "response body exceeds the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Maps an HTTP status and body onto a typed result or a [`JitoError`].
fn decode_response<T: DeserializeOwned>(status: StatusCode, body: &[u8]) -> Result<T, JitoError> {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(JitoError::RateLimited);
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(JitoError::Unauthorized {
            status: status.as_u16(),
        });
    }
    match serde_json::from_slice::<RpcEnvelope<T>>(body) {
        Ok(RpcEnvelope {
            error: Some(error), ..
        }) => Err(JitoError::Rpc {
            code: error.code,
            message: sanitize_remote_message(&error.message),
        }),
        Ok(RpcEnvelope {
            result: Some(result),
            ..
        }) if status.is_success() => Ok(result),
        Ok(_) if status.is_success() => Err(JitoError::InvalidResponse("missing JSON-RPC result")),
        _ if status.is_success() => Err(JitoError::InvalidResponse(
            "body is not the expected JSON-RPC document",
        )),
        _ => Err(JitoError::Api {
            status: status.as_u16(),
            message: sanitize_remote_message(&String::from_utf8_lossy(body)),
        }),
    }
}
