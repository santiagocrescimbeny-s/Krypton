//! Resilient Geyser stream supervisor.
//!
//! This module owns everything about consuming a Yellowstone-style account and
//! instruction stream **except the wire protocol**. The protocol itself (HTTP/2,
//! protobuf, `x-token` metadata) lives behind two small traits,
//! [`StreamTransport`] and [`EventStream`], so that:
//!
//! - the supervision logic (reconnection with exponential backoff and jitter,
//!   idle-stream watchdog, ordered shutdown, health reporting) is complete,
//!   deterministic and tested with mocked streams;
//! - a Yellowstone gRPC adapter is a thin implementation of those traits (see
//!   `docs/yellowstone-adapter.md`) that maps `SubscribeUpdate` messages onto
//!   [`StreamUpdate`] and gRPC status codes onto
//!   [`StreamErrorKind`](crate::errors::StreamErrorKind).
//!
//! # Concurrency model
//! [`GeyserMonitor::run`] is a single task. It pushes decoded events into a
//! bounded [`tokio::sync::mpsc`] channel (backpressure instead of unbounded
//! memory), publishes health through a [`tokio::sync::watch`] channel (readers
//! never block the stream) and listens to a [`ShutdownSignal`]. Every `.await`
//! on the hot path is raced against the shutdown signal, so the task cannot be
//! stuck behind a slow consumer or a dead socket. Consumers never call back into
//! the monitor, so there is no lock and no cycle to deadlock on.
//!
//! # Zero copy
//! Account payloads travel as [`bytes::Bytes`]: reference-counted views over the
//! buffer the transport received, cloned in O(1). Decoding
//! ([`BondingCurveState::decode`](super::curve::BondingCurveState::decode))
//! reads fixed offsets straight from that buffer.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use solana_pubkey::Pubkey;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, timeout, Instant};
use tracing::{debug, info, warn};

use super::ShutdownSignal;
use crate::builder::constants::{DISCRIMINATORS_BUY, DISCRIMINATORS_SELL};
use crate::builder::constants::{DISCRIMINATOR_CREATE, DISCRIMINATOR_CREATE_V2};
use crate::errors::MonitorError;

// --- Events ------------------------------------------------------------------------

/// An account write observed on the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUpdate {
    /// Slot of the write.
    pub slot: u64,
    /// Geyser write version (monotonic per account; orders writes in a slot).
    pub write_version: u64,
    /// The account.
    pub pubkey: Pubkey,
    /// Program that owns the account.
    pub owner: Pubkey,
    /// Lamport balance after the write.
    pub lamports: u64,
    /// Account data (a shared view of the received buffer).
    pub data: Bytes,
}

/// A coarse classification of a Pump.fun instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpInstructionKind {
    /// Any buy instruction (`buy`, `buy_v2`, `buy_v3`, exact-in variants).
    Buy,
    /// Any sell instruction (`sell`, `sell_v2`, `sell_v3`).
    Sell,
    /// `create` or `create_v2`.
    Create,
    /// Anything else (admin, fee sweeps, migration, ...).
    Other,
}

/// An instruction executed against a monitored account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionUpdate {
    /// Slot of the transaction.
    pub slot: u64,
    /// Program the instruction targets.
    pub program_id: Pubkey,
    /// Instruction data (a shared view of the received buffer).
    pub data: Bytes,
}

impl InstructionUpdate {
    /// Classifies the instruction by its Anchor discriminator. Instructions
    /// shorter than a discriminator, or unknown ones, are [`PumpInstructionKind::Other`].
    #[must_use]
    pub fn kind(&self) -> PumpInstructionKind {
        let Some(head) = self.data.get(..8) else {
            return PumpInstructionKind::Other;
        };
        if DISCRIMINATORS_BUY.iter().any(|d| d.as_slice() == head) {
            PumpInstructionKind::Buy
        } else if DISCRIMINATORS_SELL.iter().any(|d| d.as_slice() == head) {
            PumpInstructionKind::Sell
        } else if head == DISCRIMINATOR_CREATE.as_slice()
            || head == DISCRIMINATOR_CREATE_V2.as_slice()
        {
            PumpInstructionKind::Create
        } else {
            PumpInstructionKind::Other
        }
    }
}

/// One decoded message of the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamUpdate {
    /// A monitored account changed.
    Account(AccountUpdate),
    /// An instruction touched a monitored account.
    Instruction(InstructionUpdate),
    /// The cluster advanced to `slot`.
    Slot(u64),
    /// Keep-alive (gRPC ping / pong). Proves liveness, carries nothing else.
    Heartbeat,
}

// --- Subscription --------------------------------------------------------------------

/// Commitment level requested for the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamCommitment {
    /// Fastest, may be rolled back. The default: the exit window is seconds long.
    #[default]
    Processed,
    /// Optimistically confirmed.
    Confirmed,
    /// Rooted.
    Finalized,
}

/// What the transport should subscribe to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionRequest {
    /// Accounts to watch (bonding curve, the wallet's token account, ...).
    pub accounts: Vec<Pubkey>,
    /// Program whose instructions touching `accounts` should be streamed.
    pub instruction_program: Option<Pubkey>,
    /// Requested commitment.
    pub commitment: StreamCommitment,
}

// --- Transport traits -------------------------------------------------------------------

/// A live stream of decoded updates.
///
/// # Cancel safety
/// [`EventStream::next_update`] is dropped mid-poll when the shutdown signal or
/// the idle watchdog fires, so it must not lose a message that was already
/// removed from the underlying socket (gRPC `Streaming::message` is cancel safe).
pub trait EventStream: Send {
    /// Waits for the next update. `Ok(None)` means the server closed the stream.
    fn next_update(
        &mut self,
    ) -> impl Future<Output = Result<Option<StreamUpdate>, MonitorError>> + Send;
}

/// Opens [`EventStream`]s. One value is reused across reconnections.
pub trait StreamTransport: Send {
    /// The stream type this transport produces.
    type Stream: EventStream;

    /// Connects and subscribes. Called again after every failure.
    fn connect(
        &mut self,
        request: &SubscriptionRequest,
    ) -> impl Future<Output = Result<Self::Stream, MonitorError>> + Send;
}

// --- Backoff ------------------------------------------------------------------------------

/// Exponential backoff parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    /// Delay after the first failure.
    pub initial: Duration,
    /// Upper bound of any delay.
    pub max: Duration,
    /// Randomization, in thousandths: a delay `d` becomes a value in
    /// `[d * (1 - jitter), d]`. `0` disables jitter; at most `1000`.
    pub jitter_permille: u16,
}

impl Default for BackoffPolicy {
    /// 50 ms doubling up to 5 s with 20 % jitter: fast first retry (the exit
    /// window is short) without hammering a struggling endpoint.
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(50),
            max: Duration::from_secs(5),
            jitter_permille: 200,
        }
    }
}

impl BackoffPolicy {
    fn validate(&self) -> Result<(), MonitorError> {
        if self.initial.is_zero() {
            return Err(MonitorError::InvalidConfig {
                reason: "backoff initial delay must be positive",
            });
        }
        if self.max < self.initial {
            return Err(MonitorError::InvalidConfig {
                reason: "backoff maximum must not be below the initial delay",
            });
        }
        if self.jitter_permille > 1_000 {
            return Err(MonitorError::InvalidConfig {
                reason: "backoff jitter must be at most 1000 permille",
            });
        }
        Ok(())
    }
}

/// Backoff state: counts consecutive failures and produces the next delay.
///
/// The jitter source is a small xorshift generator (no `rand` dependency, no
/// global state, reproducible from the seed in tests).
#[derive(Debug, Clone)]
pub struct Backoff {
    policy: BackoffPolicy,
    attempt: u32,
    rng: u64,
}

impl Backoff {
    /// Creates a backoff; `seed` only influences the jitter.
    #[must_use]
    pub fn new(policy: BackoffPolicy, seed: u64) -> Self {
        Self {
            policy,
            attempt: 0,
            // xorshift must not start at zero.
            rng: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// Consecutive failures counted so far.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// Forgets all failures (the connection proved stable).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    fn next_random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// The deterministic part of the delay for the current attempt:
    /// `min(max, initial * 2^attempt)`.
    #[must_use]
    pub fn base_delay(&self) -> Duration {
        let max_ms = self.policy.max.as_millis();
        let mut delay_ms = self.policy.initial.as_millis();
        // Doubling stops at `max`, so the loop is bounded by log2(max / initial).
        for _ in 0..self.attempt {
            if delay_ms >= max_ms {
                break;
            }
            delay_ms = delay_ms.saturating_mul(2);
        }
        let capped = delay_ms.min(max_ms);
        Duration::from_millis(u64::try_from(capped).unwrap_or(u64::MAX))
    }

    /// Returns the delay to wait before the next attempt and counts the failure.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.base_delay();
        self.attempt = self.attempt.saturating_add(1);
        let jitter = u128::from(self.policy.jitter_permille);
        if jitter == 0 {
            return base;
        }
        let base_ms = base.as_millis();
        // Reduction in [0, base * jitter / 1000].
        let max_reduction = base_ms.saturating_mul(jitter) / 1_000;
        let reduction = if max_reduction == 0 {
            0
        } else {
            u128::from(self.next_random()) % (max_reduction.saturating_add(1))
        };
        let delay_ms = base_ms.saturating_sub(reduction);
        Duration::from_millis(u64::try_from(delay_ms).unwrap_or(u64::MAX))
    }
}

// --- Settings & health ------------------------------------------------------------------------

/// Supervisor settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorSettings {
    /// Reconnection backoff.
    pub backoff: BackoffPolicy,
    /// Maximum time to establish a connection and subscription.
    pub connect_timeout: Duration,
    /// Maximum silence (no update, no heartbeat) before the stream is declared dead.
    pub idle_timeout: Duration,
    /// A connection that lived at least this long is considered stable: its
    /// failure resets the backoff instead of extending it.
    pub stable_after: Duration,
    /// Consecutive failed connections tolerated before giving up (`None` = never).
    pub max_consecutive_failures: Option<u32>,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            backoff: BackoffPolicy::default(),
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(2),
            stable_after: Duration::from_secs(5),
            max_consecutive_failures: Some(20),
        }
    }
}

impl MonitorSettings {
    /// Checks the settings are internally consistent.
    ///
    /// # Errors
    /// [`MonitorError::InvalidConfig`] for zero timeouts or a bad backoff.
    pub fn validate(&self) -> Result<(), MonitorError> {
        self.backoff.validate()?;
        if self.connect_timeout.is_zero() || self.idle_timeout.is_zero() {
            return Err(MonitorError::InvalidConfig {
                reason: "timeouts must be positive",
            });
        }
        Ok(())
    }
}

/// Where the supervisor is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Not started yet.
    #[default]
    Idle,
    /// Opening a connection.
    Connecting,
    /// Streaming.
    Connected,
    /// Waiting before the next attempt.
    Backoff,
    /// Finished (shutdown, consumer gone, or fatal error).
    Stopped,
}

/// Health snapshot published through a [`watch`] channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HealthStatus {
    /// Current life-cycle state.
    pub state: ConnectionState,
    /// Successful connections after the first one.
    pub reconnects: u64,
    /// Updates forwarded to the consumer.
    pub events_forwarded: u64,
    /// Consecutive failed connections right now.
    pub consecutive_failures: u32,
    /// Highest slot seen on the stream.
    pub last_slot: u64,
}

// --- Supervisor -------------------------------------------------------------------------------

/// How a streaming session ended.
enum SessionEnd {
    Shutdown,
    ConsumerGone,
    Failed(MonitorError),
}

/// Reconnecting stream supervisor. See the module documentation.
#[derive(Debug)]
pub struct GeyserMonitor<T> {
    transport: T,
    request: SubscriptionRequest,
    settings: MonitorSettings,
    seed: u64,
}

impl<T: StreamTransport> GeyserMonitor<T> {
    /// Validates `settings` and builds the supervisor.
    ///
    /// # Errors
    /// [`MonitorError::InvalidConfig`] for inconsistent settings or an empty
    /// account list.
    pub fn new(
        transport: T,
        request: SubscriptionRequest,
        settings: MonitorSettings,
    ) -> Result<Self, MonitorError> {
        settings.validate()?;
        if request.accounts.is_empty() {
            return Err(MonitorError::InvalidConfig {
                reason: "subscription needs at least one account",
            });
        }
        Ok(Self {
            transport,
            request,
            settings,
            seed: 0,
        })
    }

    /// Seeds the backoff jitter (tests use it for reproducible delays).
    #[must_use]
    pub fn with_jitter_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Runs until shutdown, until the consumer drops its receiver, or until a
    /// non-retryable failure / exhausted retry budget.
    ///
    /// Returns the final [`HealthStatus`] on a clean stop.
    ///
    /// # Errors
    /// - the first non-retryable [`MonitorError`] (for example rejected credentials);
    /// - [`MonitorError::ReconnectExhausted`] after `max_consecutive_failures`.
    pub async fn run(
        mut self,
        events: mpsc::Sender<StreamUpdate>,
        health: watch::Sender<HealthStatus>,
        mut shutdown: ShutdownSignal,
    ) -> Result<HealthStatus, MonitorError> {
        let mut backoff = Backoff::new(self.settings.backoff, self.seed);
        let mut status = HealthStatus::default();
        let mut ever_connected = false;

        let result = loop {
            if shutdown.is_triggered() {
                break Ok(());
            }
            status.state = ConnectionState::Connecting;
            publish(&health, status);

            let connect = timeout(
                self.settings.connect_timeout,
                self.transport.connect(&self.request),
            );
            let connected = tokio::select! {
                biased;
                () = shutdown.wait() => break Ok(()),
                result = connect => result,
            };

            let failure = match connected {
                Ok(Ok(stream)) => {
                    if ever_connected {
                        status.reconnects = status.reconnects.saturating_add(1);
                    }
                    ever_connected = true;
                    status.state = ConnectionState::Connected;
                    publish(&health, status);
                    info!(reconnects = status.reconnects, "geyser stream connected");

                    let connected_at = Instant::now();
                    match self
                        .stream_session(stream, &events, &health, &mut status, &mut shutdown)
                        .await
                    {
                        SessionEnd::Shutdown | SessionEnd::ConsumerGone => break Ok(()),
                        SessionEnd::Failed(err) => {
                            if connected_at.elapsed() >= self.settings.stable_after {
                                backoff.reset();
                                status.consecutive_failures = 0;
                            }
                            err
                        }
                    }
                }
                Ok(Err(err)) => err,
                Err(_elapsed) => MonitorError::ConnectTimeout {
                    millis: millis_u64(self.settings.connect_timeout),
                },
            };

            if !failure.is_retryable() {
                warn!(error = %failure, "geyser stream failed with a non-retryable error");
                break Err(failure);
            }
            status.consecutive_failures = status.consecutive_failures.saturating_add(1);
            if let Some(max) = self.settings.max_consecutive_failures {
                if status.consecutive_failures > max {
                    break Err(MonitorError::ReconnectExhausted {
                        attempts: status.consecutive_failures,
                    });
                }
            }
            let delay = backoff.next_delay();
            warn!(
                error = %failure,
                attempt = status.consecutive_failures,
                delay_ms = millis_u64(delay),
                "geyser stream lost; reconnecting"
            );
            status.state = ConnectionState::Backoff;
            publish(&health, status);
            tokio::select! {
                biased;
                () = shutdown.wait() => break Ok(()),
                () = sleep(delay) => {}
            }
        };

        status.state = ConnectionState::Stopped;
        publish(&health, status);
        result.map(|()| status)
    }

    /// Pumps one connected stream until it fails or the run is stopping.
    async fn stream_session(
        &self,
        mut stream: T::Stream,
        events: &mpsc::Sender<StreamUpdate>,
        health: &watch::Sender<HealthStatus>,
        status: &mut HealthStatus,
        shutdown: &mut ShutdownSignal,
    ) -> SessionEnd {
        let idle = self.settings.idle_timeout;
        loop {
            let next = timeout(idle, stream.next_update());
            let received = tokio::select! {
                biased;
                () = shutdown.wait() => return SessionEnd::Shutdown,
                received = next => received,
            };
            let update = match received {
                Err(_elapsed) => {
                    return SessionEnd::Failed(MonitorError::IdleTimeout {
                        millis: millis_u64(idle),
                    })
                }
                Ok(Err(err)) => return SessionEnd::Failed(err),
                Ok(Ok(None)) => return SessionEnd::Failed(MonitorError::StreamClosed),
                Ok(Ok(Some(update))) => update,
            };

            if let Some(slot) = slot_of(&update) {
                status.last_slot = status.last_slot.max(slot);
            }
            if update == StreamUpdate::Heartbeat {
                debug!("geyser heartbeat");
                continue;
            }

            // Backpressure: wait for room, but never past a shutdown request.
            let sent = tokio::select! {
                biased;
                () = shutdown.wait() => return SessionEnd::Shutdown,
                sent = events.send(update) => sent,
            };
            if sent.is_err() {
                return SessionEnd::ConsumerGone;
            }
            status.events_forwarded = status.events_forwarded.saturating_add(1);
            publish(health, *status);
        }
    }
}

fn slot_of(update: &StreamUpdate) -> Option<u64> {
    match update {
        StreamUpdate::Account(account) => Some(account.slot),
        StreamUpdate::Instruction(instruction) => Some(instruction.slot),
        StreamUpdate::Slot(slot) => Some(*slot),
        StreamUpdate::Heartbeat => None,
    }
}

fn publish(health: &watch::Sender<HealthStatus>, status: HealthStatus) {
    // `send_replace` never fails, even without receivers.
    health.send_replace(status);
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
