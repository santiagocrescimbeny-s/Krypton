//! Yellowstone gRPC adapter (Phase 8): everything between the wire protocol and
//! the [`StreamTransport`] the exit pipeline consumes.
//!
//! # What lives here, and what does not
//! The supervision logic (reconnection with exponential backoff and jitter, the
//! idle watchdog, health reporting, ordered shutdown) is in
//! [`GeyserMonitor`](crate::monitor::geyser::GeyserMonitor) and is **not**
//! duplicated: [`YellowstoneTransport`] is the transport it reconnects through.
//! This module owns the rest of the adapter:
//!
//! - [`build_wire_request`]: [`SubscriptionRequest`] -> the Yellowstone
//!   `SubscribeRequest` (account filter, transaction filter, slots, commitment,
//!   ping);
//! - [`convert_message`]: `SubscribeUpdate` -> [`StreamUpdate`], including the
//!   v0-transaction address resolution (static keys, then loaded writable, then
//!   loaded readonly) that decides which program an instruction targets;
//! - [`YellowstoneStream`]: an [`EventStream`] that unpacks one transaction into
//!   several instruction updates, answers the server's pings and never panics on
//!   a malformed message;
//! - [`classify_grpc_code`]: gRPC status codes -> [`StreamErrorKind`] (so a
//!   rejected token is never retried and an overloaded endpoint is);
//! - [`YellowstoneSettings`]: the endpoint and the `x-token`, validated, with a
//!   `Debug` that prints neither.
//!
//! # The one thing that is not here: `tonic`
//! The network call itself sits behind two tiny traits, [`YellowstoneLink`] and
//! [`YellowstoneSession`]. Their implementation needs `tonic`, `prost` and the
//! generated Yellowstone protobufs, none of which are in `Cargo.lock`, and the
//! dependency set may only change where crates.io is reachable (the lockfile is
//! never edited by hand). The wire types below ([`WireRequest`], [`WireMessage`])
//! are plain-Rust mirrors of the handful of protobuf fields the pipeline uses, so
//! that the binding is a field-for-field copy and every decision lives in code
//! that is compiled and tested without the network. See
//! `docs/yellowstone-adapter.md` for the binding checklist.
//!
//! # Zero panics
//! Pubkeys arrive as byte vectors; a wrong length, an out-of-range account index
//! or an oversized transaction is a [`StreamErrorKind::Protocol`] error (the
//! monitor reconnects), never a panic and never a partial update.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;

use bytes::Bytes;
use solana_pubkey::Pubkey;

use crate::errors::{MonitorError, StreamErrorKind, YellowstoneError};
use crate::monitor::geyser::{
    AccountUpdate, EventStream, InstructionUpdate, StreamCommitment, StreamTransport, StreamUpdate,
    SubscriptionRequest,
};
use crate::security::SecretString;

// --- Limits ---------------------------------------------------------------------------

/// Bytes in a public key.
pub const PUBKEY_BYTES: usize = 32;

/// Longest endpoint accepted, in bytes.
pub const MAX_ENDPOINT_BYTES: usize = 2_048;

/// Most instructions (outer plus inner) accepted in one transaction message. A
/// legacy or v0 transaction is at most 1 232 bytes, so a larger count is a
/// protocol violation; the cap also bounds the stream's internal queue.
pub const MAX_INSTRUCTIONS_PER_TRANSACTION: usize = 1_024;

/// The id the adapter puts in its own `SubscribeRequestPing`.
pub const PING_ID: i32 = 1;

// --- Settings -------------------------------------------------------------------------

/// Where to connect and how to authenticate.
///
/// `Debug` prints the scheme, host and port only, and whether a token is set:
/// providers often embed an access token in the URL path, and the `x-token` is a
/// secret.
pub struct YellowstoneSettings {
    endpoint: String,
    x_token: Option<SecretString>,
}

impl YellowstoneSettings {
    /// Validates `endpoint` and an optional `x_token`.
    ///
    /// # Errors
    /// [`YellowstoneError::InvalidEndpoint`] for an endpoint that is not `https`
    /// (plain `http` is accepted only for loopback, for tests), carries
    /// credentials, has no host or an invalid port;
    /// [`YellowstoneError::InvalidToken`] for a blank or malformed token.
    pub fn new(endpoint: &str, x_token: Option<SecretString>) -> Result<Self, YellowstoneError> {
        let endpoint = endpoint.trim();
        validate_endpoint(endpoint)?;
        if let Some(token) = &x_token {
            validate_token(token.expose_secret())?;
        }
        Ok(Self {
            endpoint: endpoint.to_owned(),
            x_token,
        })
    }

    /// The validated endpoint. Treat it as a secret: it may embed a token.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The `x-token` to send as gRPC metadata, if any.
    #[must_use]
    pub fn x_token(&self) -> Option<&SecretString> {
        self.x_token.as_ref()
    }

    /// `scheme://host[:port]` of the endpoint: safe to log.
    #[must_use]
    pub fn origin(&self) -> &str {
        origin_of(&self.endpoint)
    }
}

impl fmt::Debug for YellowstoneSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("YellowstoneSettings")
            .field("origin", &self.origin())
            .field("x_token", &self.x_token.is_some())
            .finish()
    }
}

fn reject(reason: &'static str) -> YellowstoneError {
    YellowstoneError::InvalidEndpoint { reason }
}

/// Splits `scheme://rest`, lower-casing nothing: the scheme is compared
/// case-insensitively by the callers.
fn split_scheme(endpoint: &str) -> Option<(&str, &str)> {
    endpoint.split_once("://")
}

/// The authority component: everything up to the first `/`, `?` or `#`.
fn authority_of(rest: &str) -> &str {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest.get(..end).unwrap_or("")
}

/// `scheme://authority` of a validated endpoint (the whole string if the shape
/// is unexpected, which validation rules out).
fn origin_of(endpoint: &str) -> &str {
    let Some((scheme, rest)) = split_scheme(endpoint) else {
        return "";
    };
    let authority_len = authority_of(rest).len();
    let end = scheme.len().saturating_add(3).saturating_add(authority_len);
    endpoint.get(..end).unwrap_or("")
}

/// Splits an authority into `(host, port)`; the host keeps its brackets for IPv6.
fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let host = authority.get(..=close)?;
        let tail = authority.get(close.saturating_add(1)..)?;
        return match tail {
            "" => Some((host, None)),
            _ => Some((host, Some(tail.strip_prefix(':')?))),
        };
    }
    match authority.split_once(':') {
        Some((host, port)) => Some((host, Some(port))),
        None => Some((authority, None)),
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]"
}

fn validate_endpoint(endpoint: &str) -> Result<(), YellowstoneError> {
    if endpoint.is_empty() {
        return Err(reject("is empty"));
    }
    if endpoint.len() > MAX_ENDPOINT_BYTES {
        return Err(reject("is too long"));
    }
    if endpoint
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(reject("contains whitespace or control characters"));
    }
    let Some((scheme, rest)) = split_scheme(endpoint) else {
        return Err(reject("has no scheme"));
    };
    let secure = scheme.eq_ignore_ascii_case("https");
    if !secure && !scheme.eq_ignore_ascii_case("http") {
        return Err(reject("scheme must be https"));
    }
    let authority = authority_of(rest);
    if authority.contains('@') {
        return Err(reject("must not carry credentials"));
    }
    let Some((host, port)) = split_host_port(authority) else {
        return Err(reject("has an invalid host"));
    };
    if host.is_empty() || host == "[]" {
        return Err(reject("has no host"));
    }
    if !secure && !is_loopback(host) {
        return Err(reject("plain http is only allowed for loopback"));
    }
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(value) if value != 0 => {}
            _ => return Err(reject("has an invalid port")),
        }
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), YellowstoneError> {
    if token.trim().is_empty() {
        return Err(YellowstoneError::InvalidToken { reason: "is blank" });
    }
    if token.len() > MAX_ENDPOINT_BYTES {
        return Err(YellowstoneError::InvalidToken {
            reason: "is too long",
        });
    }
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        // gRPC metadata values cannot carry them, and a token never has them.
        return Err(YellowstoneError::InvalidToken {
            reason: "contains whitespace or control characters",
        });
    }
    Ok(())
}

// --- gRPC status classification ---------------------------------------------------------------

/// Maps a gRPC status code (`tonic::Code as i32`) onto a [`StreamErrorKind`].
///
/// - `UNAUTHENTICATED` (16) and `PERMISSION_DENIED` (7): [`Unauthenticated`], never retried;
/// - `INVALID_ARGUMENT` (3) and `UNIMPLEMENTED` (12): [`InvalidRequest`], never retried
///   (a malformed subscription or an endpoint without the method does not heal);
/// - `RESOURCE_EXHAUSTED` (8) and `UNAVAILABLE` (14): [`Overloaded`];
/// - `INTERNAL` (13) and `DATA_LOSS` (15): [`Protocol`];
/// - anything else: [`Other`].
///
/// [`Unauthenticated`]: StreamErrorKind::Unauthenticated
/// [`InvalidRequest`]: StreamErrorKind::InvalidRequest
/// [`Overloaded`]: StreamErrorKind::Overloaded
/// [`Protocol`]: StreamErrorKind::Protocol
/// [`Other`]: StreamErrorKind::Other
#[must_use]
pub fn classify_grpc_code(code: i32) -> StreamErrorKind {
    match code {
        7 | 16 => StreamErrorKind::Unauthenticated,
        3 | 12 => StreamErrorKind::InvalidRequest,
        8 | 14 => StreamErrorKind::Overloaded,
        13 | 15 => StreamErrorKind::Protocol,
        _ => StreamErrorKind::Other,
    }
}

/// The [`MonitorError`] for a failed gRPC call with status `code`.
#[must_use]
pub fn grpc_failure(code: i32) -> MonitorError {
    MonitorError::Connection {
        kind: classify_grpc_code(code),
    }
}

fn protocol_violation() -> MonitorError {
    MonitorError::Connection {
        kind: StreamErrorKind::Protocol,
    }
}

// --- The request ------------------------------------------------------------------------------

/// The Yellowstone `SubscribeRequest` the adapter sends, reduced to the fields it
/// uses. The binding copies it into the protobuf message one field at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRequest {
    /// `accounts["krypton"].account`: base58 keys of the accounts to stream. No
    /// `owner` and no `memcmp` filters.
    pub accounts: Vec<String>,
    /// `transactions["krypton"].account_include`: base58 keys. Empty when no
    /// transaction stream is wanted (then no transaction filter is sent at all).
    pub transaction_account_include: Vec<String>,
    /// `transactions["krypton"].vote`: always `Some(false)`; a vote is never wanted.
    pub exclude_votes: bool,
    /// `transactions["krypton"].failed`: always `Some(false)`; failed
    /// transactions change nothing on the curve.
    pub exclude_failed: bool,
    /// `slots["krypton"]`: enabled, a second liveness signal.
    pub include_slots: bool,
    /// `commitment` as the protobuf `CommitmentLevel` value
    /// (`0` processed, `1` confirmed, `2` finalized).
    pub commitment: i32,
    /// `ping`: the id of the `SubscribeRequestPing` sent with the request.
    pub ping_id: Option<i32>,
}

/// Name of the single filter of each kind in the request.
pub const FILTER_NAME: &str = "krypton";

/// The protobuf `CommitmentLevel` value for `commitment`.
#[must_use]
pub fn commitment_code(commitment: StreamCommitment) -> i32 {
    match commitment {
        StreamCommitment::Processed => 0,
        StreamCommitment::Confirmed => 1,
        StreamCommitment::Finalized => 2,
    }
}

/// Builds the wire request for `request`.
///
/// Transactions are streamed for the watched accounts (the bonding curve and the
/// wallet's token account), which is what the exit pipeline needs. With
/// `program_wide` the Pump program id is added to the transaction filter as
/// well, which also delivers **every** `create`/`buy`/`sell` of the program (mint
/// creations included) at a much higher message rate: opt in only for a consumer
/// that wants the firehose.
#[must_use]
pub fn build_wire_request(request: &SubscriptionRequest, program_wide: bool) -> WireRequest {
    let accounts: Vec<String> = request.accounts.iter().map(ToString::to_string).collect();
    let mut transaction_account_include = Vec::new();
    if request.instruction_program.is_some() {
        transaction_account_include.clone_from(&accounts);
        if program_wide {
            if let Some(program) = request.instruction_program {
                transaction_account_include.push(program.to_string());
            }
        }
    }
    WireRequest {
        accounts,
        transaction_account_include,
        exclude_votes: true,
        exclude_failed: true,
        include_slots: true,
        commitment: commitment_code(request.commitment),
        ping_id: Some(PING_ID),
    }
}

// --- The messages -------------------------------------------------------------------------------

/// An account write (`SubscribeUpdateAccount` with its account info).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireAccount {
    /// Slot of the write.
    pub slot: u64,
    /// Account key (32 bytes).
    pub pubkey: Vec<u8>,
    /// Owner program (32 bytes).
    pub owner: Vec<u8>,
    /// Lamport balance after the write.
    pub lamports: u64,
    /// Geyser write version.
    pub write_version: u64,
    /// Account data.
    pub data: Vec<u8>,
}

/// A compiled instruction (outer or inner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireInstruction {
    /// Index of the program id in the transaction's full account list.
    pub program_id_index: u32,
    /// Instruction data.
    pub data: Vec<u8>,
}

/// A transaction (`SubscribeUpdateTransaction` flattened to what is used).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireTransaction {
    /// Slot of the transaction.
    pub slot: u64,
    /// `message.account_keys`: the static keys.
    pub account_keys: Vec<Vec<u8>>,
    /// `meta.loaded_writable_addresses` (v0 lookup tables).
    pub loaded_writable: Vec<Vec<u8>>,
    /// `meta.loaded_readonly_addresses` (v0 lookup tables).
    pub loaded_readonly: Vec<Vec<u8>>,
    /// `message.instructions`.
    pub instructions: Vec<WireInstruction>,
    /// `meta.inner_instructions`, flattened in execution order.
    pub inner_instructions: Vec<WireInstruction>,
}

/// One server message (`SubscribeUpdate.update_oneof`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage {
    /// An account write.
    Account(WireAccount),
    /// A transaction.
    Transaction(WireTransaction),
    /// A slot notification.
    Slot(u64),
    /// The server's ping: the client must answer or the server closes the stream.
    Ping,
    /// The server's answer to the client's ping.
    Pong,
    /// Any other update type (block meta, entry, ...): ignored, but it proves the
    /// stream is alive.
    Unsupported,
}

/// What the stream must do after a message was converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Followup {
    /// Nothing.
    None,
    /// Answer the server's ping.
    AnswerPing,
}

fn pubkey_from(bytes: &[u8]) -> Result<Pubkey, MonitorError> {
    <[u8; PUBKEY_BYTES]>::try_from(bytes)
        .map(Pubkey::new_from_array)
        .map_err(|_| protocol_violation())
}

/// The key at `index` of the transaction's full account list: static keys, then
/// loaded writable, then loaded readonly (the order the runtime uses).
fn key_at(tx: &WireTransaction, index: u32) -> Option<&Vec<u8>> {
    let mut remaining = usize::try_from(index).ok()?;
    for list in [&tx.account_keys, &tx.loaded_writable, &tx.loaded_readonly] {
        if let Some(key) = list.get(remaining) {
            return Some(key);
        }
        remaining = remaining.saturating_sub(list.len());
    }
    None
}

/// Converts `message` into stream updates, appended to `out`.
///
/// - an account write becomes [`StreamUpdate::Account`] (the data buffer is
///   *moved* into a [`Bytes`], no copy);
/// - a transaction becomes one [`StreamUpdate::Instruction`] per instruction,
///   outer first and then inner, that targets `program` (all of them when
///   `program` is `None`);
/// - slots, pongs and unsupported updates become [`StreamUpdate::Slot`] /
///   [`StreamUpdate::Heartbeat`];
/// - a ping becomes a heartbeat and asks to be answered ([`Followup::AnswerPing`]).
///
/// On error nothing is appended.
///
/// # Errors
/// [`MonitorError::Connection`] with [`StreamErrorKind::Protocol`] for a key that
/// is not 32 bytes, a program index outside the account list, or a transaction
/// with more than [`MAX_INSTRUCTIONS_PER_TRANSACTION`] instructions.
pub fn convert_message(
    message: WireMessage,
    program: Option<Pubkey>,
    out: &mut VecDeque<StreamUpdate>,
) -> Result<Followup, MonitorError> {
    match message {
        WireMessage::Account(account) => {
            let update = AccountUpdate {
                slot: account.slot,
                write_version: account.write_version,
                pubkey: pubkey_from(&account.pubkey)?,
                owner: pubkey_from(&account.owner)?,
                lamports: account.lamports,
                data: Bytes::from(account.data),
            };
            out.push_back(StreamUpdate::Account(update));
            Ok(Followup::None)
        }
        WireMessage::Transaction(tx) => {
            convert_transaction(&tx, program, out)?;
            Ok(Followup::None)
        }
        WireMessage::Slot(slot) => {
            out.push_back(StreamUpdate::Slot(slot));
            Ok(Followup::None)
        }
        WireMessage::Ping => {
            out.push_back(StreamUpdate::Heartbeat);
            Ok(Followup::AnswerPing)
        }
        WireMessage::Pong | WireMessage::Unsupported => {
            out.push_back(StreamUpdate::Heartbeat);
            Ok(Followup::None)
        }
    }
}

fn convert_transaction(
    tx: &WireTransaction,
    program: Option<Pubkey>,
    out: &mut VecDeque<StreamUpdate>,
) -> Result<(), MonitorError> {
    let total = tx
        .instructions
        .len()
        .saturating_add(tx.inner_instructions.len());
    if total > MAX_INSTRUCTIONS_PER_TRANSACTION {
        return Err(protocol_violation());
    }
    // Resolve everything into a local list first, so that an error appends nothing.
    let mut selected = Vec::new();
    for instruction in tx.instructions.iter().chain(tx.inner_instructions.iter()) {
        let key = key_at(tx, instruction.program_id_index).ok_or_else(protocol_violation)?;
        let program_id = pubkey_from(key)?;
        if program.is_none_or(|wanted| wanted == program_id) {
            selected.push(StreamUpdate::Instruction(InstructionUpdate {
                slot: tx.slot,
                program_id,
                data: Bytes::copy_from_slice(&instruction.data),
            }));
        }
    }
    out.extend(selected);
    Ok(())
}

// --- The link (the part that needs tonic) ---------------------------------------------------------

/// Dials a Yellowstone endpoint. Implemented by the `tonic` binding.
pub trait YellowstoneLink: Send {
    /// The open subscription.
    type Session: YellowstoneSession;

    /// Connects (TLS for `https`), authenticates with the `x-token` metadata and
    /// sends `request` as the first message of the `Subscribe` stream.
    ///
    /// The supervisor applies the connect deadline and calls this again after every
    /// failure, so it must not keep a dead channel. Failures are mapped with
    /// [`grpc_failure`] / [`classify_grpc_code`]; transport errors that never
    /// produced a status are [`StreamErrorKind::Unreachable`] or
    /// [`StreamErrorKind::Tls`].
    fn open(
        &mut self,
        settings: &YellowstoneSettings,
        request: &WireRequest,
    ) -> impl Future<Output = Result<Self::Session, MonitorError>> + Send;
}

/// An open `Subscribe` stream. Implemented by the `tonic` binding.
pub trait YellowstoneSession: Send {
    /// Waits for the next server message. `Ok(None)` means the server closed the
    /// stream.
    ///
    /// # Cancel safety
    /// Dropped mid-poll by the supervisor's watchdog and shutdown: a message that
    /// left the socket must not be lost (`tonic::Streaming::message` is cancel safe).
    fn next_message(
        &mut self,
    ) -> impl Future<Output = Result<Option<WireMessage>, MonitorError>> + Send;

    /// Sends the client's answer to a server ping: a `SubscribeRequest` whose
    /// `ping` is `{ id: PING_ID }`.
    fn answer_ping(&mut self) -> impl Future<Output = Result<(), MonitorError>> + Send;
}

// --- The transport --------------------------------------------------------------------------------

/// The Yellowstone [`StreamTransport`]: plug it into
/// [`GeyserMonitor::new`](crate::monitor::geyser::GeyserMonitor::new) (or hand it
/// to `LiveExecutor::run_exit`) and the whole exit pipeline runs on live data.
#[derive(Debug)]
pub struct YellowstoneTransport<L> {
    link: L,
    settings: YellowstoneSettings,
    program_wide: bool,
}

impl<L: YellowstoneLink> YellowstoneTransport<L> {
    /// Creates the transport. The exit-path subscription (watched accounts only)
    /// is the default.
    #[must_use]
    pub fn new(link: L, settings: YellowstoneSettings) -> Self {
        Self {
            link,
            settings,
            program_wide: false,
        }
    }

    /// Also streams every transaction of the Pump program (see
    /// [`build_wire_request`]).
    #[must_use]
    pub fn with_program_wide_transactions(mut self, enabled: bool) -> Self {
        self.program_wide = enabled;
        self
    }

    /// The settings (endpoint and token).
    #[must_use]
    pub fn settings(&self) -> &YellowstoneSettings {
        &self.settings
    }
}

impl<L: YellowstoneLink> StreamTransport for YellowstoneTransport<L> {
    type Stream = YellowstoneStream<L::Session>;

    async fn connect(
        &mut self,
        request: &SubscriptionRequest,
    ) -> Result<Self::Stream, MonitorError> {
        let wire = build_wire_request(request, self.program_wide);
        let session = self.link.open(&self.settings, &wire).await?;
        Ok(YellowstoneStream::new(session, request.instruction_program))
    }
}

/// The [`EventStream`] over an open [`YellowstoneSession`].
#[derive(Debug)]
pub struct YellowstoneStream<S> {
    session: S,
    program: Option<Pubkey>,
    pending: VecDeque<StreamUpdate>,
    ping_due: bool,
}

impl<S: YellowstoneSession> YellowstoneStream<S> {
    /// Wraps `session`; instructions are kept when they target `program`
    /// (all of them when it is `None`).
    #[must_use]
    pub fn new(session: S, program: Option<Pubkey>) -> Self {
        Self {
            session,
            program,
            pending: VecDeque::new(),
            ping_due: false,
        }
    }
}

impl<S: YellowstoneSession> EventStream for YellowstoneStream<S> {
    /// Cancel safe: converted updates and an unanswered ping live in `self`, so a
    /// dropped call loses nothing and the ping is answered by the next call.
    async fn next_update(&mut self) -> Result<Option<StreamUpdate>, MonitorError> {
        loop {
            if self.ping_due {
                self.session.answer_ping().await?;
                self.ping_due = false;
            }
            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(update));
            }
            let Some(message) = self.session.next_message().await? else {
                return Ok(None);
            };
            if convert_message(message, self.program, &mut self.pending)? == Followup::AnswerPing {
                self.ping_due = true;
            }
        }
    }
}
