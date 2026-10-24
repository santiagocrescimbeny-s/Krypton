//! The `tonic` binding of the Yellowstone adapter: [`TonicLink`] implements
//! [`YellowstoneLink`], and with it `YellowstoneTransport<TonicLink>` is the
//! production [`StreamTransport`](crate::monitor::geyser::StreamTransport).
//!
//! # STATUS: reference binding, NEVER COMPILED
//!
//! This file is written against `yellowstone-grpc-proto` and `tonic` from the
//! documentation (`docs/yellowstone-adapter.md`), in an environment where
//! crates.io is unreachable. It exists so that adding the dependencies on a
//! machine that has network access is the only step left. Expect to fix small
//! API differences between crate versions the first time it is built, and run
//! it against a real endpoint (curve account, ping, forced drop) before
//! trusting it with funds.
//!
//! It is compiled only with the `grpc` cargo feature, which ships empty so that
//! `Cargo.lock` and the default build (the three gates) are unaffected. The
//! manifest is prepared for it (audit finding #48): placeholders in
//! `[workspace.dependencies]` and commented lines in `[features]` /
//! `[dependencies]`. To enable it:
//!
//! 1. in `Cargo.toml`, pin the three placeholders (`yellowstone-grpc-proto` with
//!    the feature exposing `geyser::geyser_client`, `tonic` with TLS roots, for
//!    example `tls-native-roots`, and `tokio-stream`; versions sharing one
//!    `tonic`), uncomment the three `optional = true` dependencies and set
//!    `grpc = ["dep:yellowstone-grpc-proto", "dep:tonic", "dep:tokio-stream"]`;
//! 2. run `cargo update -w`, review and commit the `Cargo.lock` diff, run
//!    `cargo audit`;
//! 3. build with `--features grpc`.
//!
//! # Secrets
//! The `x-token` is sent as sensitive metadata and never logged or put in an
//! error. `tonic` keeps its own copy of the header value for the life of the
//! channel; that copy is not zeroized (a limit of the dependency).
//!
//! The conversion functions are pure and never panic: a missing field is a
//! protocol violation ([`StreamErrorKind::Protocol`]), not an unwrap.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use yellowstone_grpc_proto::geyser::geyser_client::GeyserClient;
use yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof;
use yellowstone_grpc_proto::geyser::{
    SubscribeRequest, SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions, SubscribeRequestPing, SubscribeUpdate,
};
use yellowstone_grpc_proto::tonic::metadata::AsciiMetadataValue;
use yellowstone_grpc_proto::tonic::transport::{ClientTlsConfig, Endpoint};
use yellowstone_grpc_proto::tonic::{Request, Streaming};

use super::yellowstone::{
    grpc_failure, WireAccount, WireInstruction, WireMessage, WireRequest, WireTransaction,
    YellowstoneLink, YellowstoneSession, YellowstoneSettings, FILTER_NAME, PING_ID,
};
use crate::errors::{MonitorError, StreamErrorKind};

/// How long the TCP/TLS handshake may take. The supervisor applies its own
/// (`MonitorSettings::connect_timeout`) around the whole `open`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// HTTP/2 keep-alive: the server drops idle subscriptions otherwise.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Depth of the client-to-server request queue (the subscription and pings).
const REQUEST_QUEUE: usize = 8;

fn failure(kind: StreamErrorKind) -> MonitorError {
    MonitorError::Connection { kind }
}

fn protocol_violation() -> MonitorError {
    failure(StreamErrorKind::Protocol)
}

/// The Yellowstone `SubscribeRequest` for `wire`.
#[must_use]
pub fn subscribe_request(wire: &WireRequest) -> SubscribeRequest {
    let mut accounts = HashMap::new();
    accounts.insert(
        FILTER_NAME.to_owned(),
        SubscribeRequestFilterAccounts {
            account: wire.accounts.clone(),
            owner: Vec::new(),
            filters: Vec::new(),
            ..Default::default()
        },
    );

    let mut transactions = HashMap::new();
    if !wire.transaction_account_include.is_empty() {
        transactions.insert(
            FILTER_NAME.to_owned(),
            SubscribeRequestFilterTransactions {
                vote: wire.exclude_votes.then_some(false),
                failed: wire.exclude_failed.then_some(false),
                account_include: wire.transaction_account_include.clone(),
                ..Default::default()
            },
        );
    }

    let mut slots = HashMap::new();
    if wire.include_slots {
        slots.insert(
            FILTER_NAME.to_owned(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(true),
                ..Default::default()
            },
        );
    }

    SubscribeRequest {
        accounts,
        transactions,
        slots,
        commitment: Some(wire.commitment),
        ping: wire.ping_id.map(|id| SubscribeRequestPing { id }),
        ..Default::default()
    }
}

/// Converts one server message. Pure: unit-testable with hand-built values.
///
/// # Errors
/// A [`StreamErrorKind::Protocol`] connection error when a mandatory part of an
/// account or transaction update is missing.
pub fn convert_update(update: SubscribeUpdate) -> Result<WireMessage, MonitorError> {
    match update.update_oneof {
        Some(UpdateOneof::Account(message)) => {
            let info = message.account.ok_or_else(protocol_violation)?;
            Ok(WireMessage::Account(WireAccount {
                slot: message.slot,
                pubkey: info.pubkey,
                owner: info.owner,
                lamports: info.lamports,
                write_version: info.write_version,
                data: info.data,
            }))
        }
        Some(UpdateOneof::Transaction(message)) => {
            let info = message.transaction.ok_or_else(protocol_violation)?;
            let transaction = info.transaction.ok_or_else(protocol_violation)?;
            let body = transaction.message.ok_or_else(protocol_violation)?;
            let meta = info.meta.ok_or_else(protocol_violation)?;
            Ok(WireMessage::Transaction(WireTransaction {
                slot: message.slot,
                account_keys: body.account_keys,
                loaded_writable: meta.loaded_writable_addresses,
                loaded_readonly: meta.loaded_readonly_addresses,
                instructions: body
                    .instructions
                    .into_iter()
                    .map(|instruction| WireInstruction {
                        program_id_index: instruction.program_id_index,
                        data: instruction.data,
                    })
                    .collect(),
                inner_instructions: meta
                    .inner_instructions
                    .into_iter()
                    .flat_map(|group| group.instructions)
                    .map(|instruction| WireInstruction {
                        program_id_index: instruction.program_id_index,
                        data: instruction.data,
                    })
                    .collect(),
            }))
        }
        Some(UpdateOneof::Slot(message)) => Ok(WireMessage::Slot(message.slot)),
        Some(UpdateOneof::Ping(_)) => Ok(WireMessage::Ping),
        Some(UpdateOneof::Pong(_)) => Ok(WireMessage::Pong),
        _ => Ok(WireMessage::Unsupported),
    }
}

/// Dials Yellowstone with `tonic`. Stateless: every [`open`](YellowstoneLink::open)
/// builds a fresh channel, so a dead one is never reused.
#[derive(Debug, Clone, Copy, Default)]
pub struct TonicLink;

impl TonicLink {
    /// A new link.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// An open `Subscribe` stream.
#[derive(Debug)]
pub struct TonicSession {
    updates: Streaming<SubscribeUpdate>,
    /// Keeps the request stream open (dropping it ends the subscription) and
    /// carries the pings.
    requests: mpsc::Sender<SubscribeRequest>,
}

impl YellowstoneLink for TonicLink {
    type Session = TonicSession;

    async fn open(
        &mut self,
        settings: &YellowstoneSettings,
        request: &WireRequest,
    ) -> Result<TonicSession, MonitorError> {
        let secure = settings
            .endpoint()
            .get(..5)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"));
        let mut endpoint = Endpoint::from_shared(settings.endpoint().to_owned())
            .map_err(|_| failure(StreamErrorKind::InvalidRequest))?
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_nodelay(true)
            .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
            .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
            .keep_alive_while_idle(true);
        if secure {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_enabled_roots())
                .map_err(|_| failure(StreamErrorKind::Tls))?;
        }
        let channel = endpoint
            .connect()
            .await
            .map_err(|_| failure(StreamErrorKind::Unreachable))?;

        // The token is optional (a loopback test server needs none). A value
        // gRPC metadata cannot carry is rejected the way a bad credential is.
        let token = match settings.x_token() {
            Some(secret) => {
                let mut value = AsciiMetadataValue::try_from(secret.expose_secret())
                    .map_err(|_| failure(StreamErrorKind::Unauthenticated))?;
                value.set_sensitive(true);
                Some(value)
            }
            None => None,
        };
        let mut client = GeyserClient::with_interceptor(channel, move |mut call: Request<()>| {
            if let Some(value) = &token {
                call.metadata_mut().insert("x-token", value.clone());
            }
            Ok(call)
        });

        let (requests, queue) = mpsc::channel::<SubscribeRequest>(REQUEST_QUEUE);
        requests
            .send(subscribe_request(request))
            .await
            .map_err(|_| failure(StreamErrorKind::Other))?;
        let response = client
            .subscribe(ReceiverStream::new(queue))
            .await
            .map_err(|status| grpc_failure(status.code() as i32))?;
        Ok(TonicSession {
            updates: response.into_inner(),
            requests,
        })
    }
}

impl YellowstoneSession for TonicSession {
    async fn next_message(&mut self) -> Result<Option<WireMessage>, MonitorError> {
        // `Streaming::message` is cancel safe: the supervisor may drop this future.
        match self.updates.message().await {
            Ok(Some(update)) => convert_update(update).map(Some),
            Ok(None) => Ok(None),
            Err(status) => Err(grpc_failure(status.code() as i32)),
        }
    }

    async fn answer_ping(&mut self) -> Result<(), MonitorError> {
        self.requests
            .send(SubscribeRequest {
                ping: Some(SubscribeRequestPing { id: PING_ID }),
                ..Default::default()
            })
            .await
            .map_err(|_| failure(StreamErrorKind::Unreachable))
    }
}
