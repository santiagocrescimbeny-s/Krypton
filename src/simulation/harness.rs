//! Dry-run simulation harness: the whole token lifecycle with no network.
//!
//! ```text
//!  Genesis launch ─▶ atomic buy ─▶ curve evolves ─▶ multi-tranche exit
//!   (LaunchBuilder)   (BuyPlan)     (MarketState)     (ExitManager + executor)
//! ```
//!
//! The harness runs the **production** pieces end to end: `LaunchBuilder` and
//! `SellBuilder` assemble and sign real transactions, `GeyserMonitor` supervises
//! the stream, `ExitManager` and `run_sell_executor` decide and dispatch. Only
//! the two network boundaries are replaced:
//!
//! - the Geyser stream is a [`SimulatedTransport`] fed by a seeded market;
//! - the Jito Block Engine is a [`SimulatedSubmitter`]: an in-memory sink that
//!   assembles and signs each sell, then drops the transaction.
//!
//! # Safety boundary
//! 1. **Structural.** This module (and `market`) imports no HTTP client, no
//!    Block Engine client and no socket. There is no code path that could
//!    broadcast a transaction or pay a tip. `tests/phase6_tests.rs` scans the
//!    source of both files to keep it that way.
//! 2. **Configuration.** [`SimulationHarness::from_config`] refuses a
//!    configuration with `DRY_RUN=false` ([`SimulationError::LiveModeRefused`]).
//! 3. **Keys.** The wallet and the mint are derived from the scenario seed, are
//!    public by construction and hold no funds. Tips are *virtual*: they are
//!    counted in the report and never leave memory.
//!
//! # Stress
//! A [`StressProfile`] injects, all from one seed so a run can be replayed:
//! background trade flow and explicit shocks (dynamic curve price impact),
//! stream jitter and an outage with a snapshot on recovery (network jitter),
//! submission latency, dropped bundles and bundles whose outcome is unknown, and
//! price movement between quote and execution (slippage: a sell fails, as on
//! chain, when the proceeds fall below `min_sol_output`).
//!
//! Every wait uses `tokio::time`, so tests run in virtual time with a paused
//! clock and a 10-second exit window takes microseconds of real time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tokio::time::{sleep, sleep_until, Instant};
use tracing::info;
use zeroize::Zeroizing;

use super::market::{
    derive_seed_bytes, generate_flow, FlowProfile, MarketLedger, MarketState, ScheduledTrade,
    SellExecution, SplitMix64, TradeFlow,
};
use crate::builder::constants::{MAX_TRANSACTION_SIZE, PUMP_PROGRAM_ID};
use crate::builder::math::{BuyPlan, CurveParams, DEFAULT_TOTAL_FEE_BPS, MAX_SLIPPAGE_BPS};
use crate::builder::sell::{SellBuilder, SellSettings};
use crate::builder::transaction::{FundingSnapshot, LaunchBuilder, LaunchParams, MintSigner};
use crate::builder::{CreateOptions, CreateVariant, LaunchMetadata};
use crate::config::Config;
use crate::errors::{ExitError, MonitorError, SimulationError, StreamErrorKind};
use crate::metrics::{MetricsSnapshot, PipelineMetrics, Stopwatch};
use crate::monitor::curve::PriceReference;
use crate::monitor::exit::{
    order_channel, report_channel, run_sell_executor, ExitEngine, ExitManager, ExitPlan,
    ExitSettings, ExitSummary, Position, SellOrder, SellOutcome, SellSubmitter,
};
use crate::monitor::geyser::{
    EventStream, GeyserMonitor, HealthStatus, MonitorSettings, StreamCommitment, StreamTransport,
    StreamUpdate, SubscriptionRequest,
};
use crate::monitor::shutdown_channel;
use crate::network::jito::{JitoTip, FALLBACK_TIP_ACCOUNT, JITO_TIP_ACCOUNTS};
use crate::security::SecureWallet;

/// Capacity of the queues between the simulated market and the stream consumer.
const QUEUE_CAPACITY: usize = 1_024;
/// Rent of the accounts `Create` allocates, assumed for the virtual funding check.
const SIMULATED_CREATE_RENT_LAMPORTS: u64 = 5_000_000;
/// Virtual balance headroom above the launch's requirement.
const SIMULATED_FUNDING_HEADROOM_LAMPORTS: u64 = 1_000_000_000;
/// Longest simulated run.
const MAX_HORIZON_MS: u64 = 120_000;
/// Longest injected delay or jitter.
const MAX_INJECTED_DELAY_MS: u64 = 10_000;
/// The monitor declares a stream dead after this much silence (its default).
const IDLE_TIMEOUT_MS: u64 = 2_000;

// --- Scenario -----------------------------------------------------------------------------------

/// A window during which the simulated stream is unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outage {
    /// Milliseconds after genesis at which the connection drops.
    pub start_ms: u64,
    /// How long reconnection attempts fail.
    pub duration_ms: u64,
}

/// The adversity injected into a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StressProfile {
    /// Background trading.
    pub flow: FlowProfile,
    /// Explicit trades on top of the background flow (a whale dumping, a pump).
    pub shocks: Vec<ScheduledTrade>,
    /// Inclusive range of the delay between a change and its stream delivery, in ms.
    pub stream_jitter_ms: (u64, u64),
    /// Inclusive range of the time a submission takes to reach the chain, in ms.
    pub submit_latency_ms: (u64, u64),
    /// Probability that a bundle is dropped and never lands, in basis points.
    pub drop_bps: u16,
    /// Probability that the outcome of a bundle is reported as unknown, in basis
    /// points. Whether such a bundle actually landed is decided by a coin flip.
    pub unknown_bps: u16,
    /// A stream outage, if any.
    pub outage: Option<Outage>,
}

impl StressProfile {
    /// A quiet market and a perfect network.
    #[must_use]
    pub fn calm() -> Self {
        Self {
            flow: FlowProfile {
                trades_per_second: 2,
                buy_bias_bps: 6_000,
                buy_lamports: (50_000_000, 200_000_000),
                sell_bps: (500, 1_500),
            },
            shocks: Vec::new(),
            stream_jitter_ms: (0, 5),
            submit_latency_ms: (20, 60),
            drop_bps: 0,
            unknown_bps: 0,
            outage: None,
        }
    }

    /// Busy two-sided trading, noticeable jitter and slow submissions, but no
    /// lost bundles.
    #[must_use]
    pub fn volatile() -> Self {
        Self {
            flow: FlowProfile {
                trades_per_second: 12,
                buy_bias_bps: 4_500,
                buy_lamports: (100_000_000, 1_500_000_000),
                sell_bps: (500, 4_000),
            },
            shocks: Vec::new(),
            stream_jitter_ms: (5, 120),
            submit_latency_ms: (80, 400),
            drop_bps: 0,
            unknown_bps: 0,
            outage: None,
        }
    }

    /// Heavy sell pressure, a whale dump, lost and unconfirmed bundles, and a
    /// stream outage in the middle of the window.
    #[must_use]
    pub fn hostile() -> Self {
        Self {
            flow: FlowProfile {
                trades_per_second: 20,
                buy_bias_bps: 3_500,
                buy_lamports: (200_000_000, 2_500_000_000),
                sell_bps: (1_000, 6_000),
            },
            shocks: vec![ScheduledTrade {
                at_ms: 4_200,
                flow: TradeFlow::SellHoldings { bps: 9_000 },
            }],
            stream_jitter_ms: (10, 300),
            submit_latency_ms: (150, 700),
            drop_bps: 1_500,
            unknown_bps: 1_500,
            outage: Some(Outage {
                start_ms: 5_000,
                duration_ms: 800,
            }),
        }
    }
}

/// Everything that defines one simulated lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioConfig {
    /// Seed of every random choice.
    pub seed: u64,
    /// Share of the supply the creator's atomic buy takes, in basis points.
    pub launch_supply_bps: u16,
    /// Assumed trading fee, in basis points.
    pub fee_bps: u16,
    /// Slippage tolerance of the creator's buy, in basis points.
    pub entry_slippage_bps: u16,
    /// Number of exit tranches spread over the 1-10 s window.
    pub exit_tranches: u8,
    /// Share of the position the tranches sell together, in basis points.
    pub exit_total_bps: u16,
    /// Stop-loss threshold (price drop from entry), in basis points.
    pub stop_loss_bps: Option<u16>,
    /// Slippage tolerance of each sell, in basis points (at most 10 %).
    pub exit_slippage_bps: u16,
    /// Virtual Jito tip of each sell, in lamports.
    pub tip_lamports: u64,
    /// Orders the exit's cost budget allows.
    pub max_exit_orders: u32,
    /// How long the market keeps trading, in milliseconds after genesis.
    pub horizon_ms: u64,
    /// Interval of the stream's keep-alive heartbeats, in milliseconds.
    pub heartbeat_ms: u64,
    /// The injected adversity.
    pub stress: StressProfile,
}

impl ScenarioConfig {
    fn base(seed: u64, stress: StressProfile) -> Self {
        Self {
            seed,
            launch_supply_bps: 975,
            fee_bps: DEFAULT_TOTAL_FEE_BPS,
            entry_slippage_bps: 500,
            exit_tranches: 4,
            exit_total_bps: 10_000,
            stop_loss_bps: None,
            exit_slippage_bps: 1_000,
            tip_lamports: 100_000,
            max_exit_orders: 16,
            horizon_ms: 12_000,
            heartbeat_ms: 500,
            stress,
        }
    }

    /// A quiet market and a perfect network.
    #[must_use]
    pub fn calm(seed: u64) -> Self {
        Self::base(seed, StressProfile::calm())
    }

    /// Busy trading and a sluggish network.
    #[must_use]
    pub fn volatile(seed: u64) -> Self {
        Self::base(seed, StressProfile::volatile())
    }

    /// Everything that can go wrong, including a 20 % stop-loss.
    #[must_use]
    pub fn hostile(seed: u64) -> Self {
        let mut scenario = Self::base(seed, StressProfile::hostile());
        scenario.stop_loss_bps = Some(2_000);
        scenario
    }

    /// Checks the scenario before anything is built.
    ///
    /// # Errors
    /// [`SimulationError::InvalidScenario`] naming the first problem.
    pub fn validate(&self) -> Result<(), SimulationError> {
        let invalid = |reason| Err(SimulationError::InvalidScenario { reason });
        if self.exit_slippage_bps > MAX_SLIPPAGE_BPS {
            return invalid("exit slippage above the 10 % cap");
        }
        if self.launch_supply_bps == 0 || self.launch_supply_bps > 10_000 {
            return invalid("launch supply share out of range");
        }
        if self.horizon_ms == 0 || self.horizon_ms > MAX_HORIZON_MS {
            return invalid("horizon out of range");
        }
        if self.heartbeat_ms == 0 || self.heartbeat_ms >= IDLE_TIMEOUT_MS {
            return invalid("heartbeat must be shorter than the stream idle timeout");
        }
        if self.max_exit_orders == 0 {
            return invalid("the exit needs room for at least one order");
        }
        let stress = &self.stress;
        if u32::from(stress.drop_bps) + u32::from(stress.unknown_bps) > 10_000 {
            return invalid("drop and unknown probabilities exceed 100 %");
        }
        if stress.stream_jitter_ms.0.max(stress.stream_jitter_ms.1) > MAX_INJECTED_DELAY_MS
            || stress.submit_latency_ms.0.max(stress.submit_latency_ms.1) > MAX_INJECTED_DELAY_MS
        {
            return invalid("injected delay above the supported maximum");
        }
        if stress.shocks.len() > super::market::MAX_GENERATED_TRADES {
            return invalid("too many shocks");
        }
        if let Some(outage) = stress.outage {
            if outage.duration_ms == 0
                || outage.start_ms.saturating_add(outage.duration_ms) > self.horizon_ms
            {
                return invalid("the outage must fit inside the horizon");
            }
        }
        Ok(())
    }
}

// --- Report -------------------------------------------------------------------------------------

/// What happened to a simulated order on its way to the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderFate {
    /// Executed.
    Landed,
    /// Reached the chain but the price had moved past `min_sol_output`.
    SlippageRejected,
    /// Reached the chain after the curve completed.
    CurveComplete,
    /// Never reached the chain.
    Dropped,
}

/// One order as the market saw it, next to what the exit engine was told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutedOrder {
    /// Order sequence.
    pub sequence: u8,
    /// Tokens offered.
    pub tokens: u64,
    /// The quote's `min_sol_output`, in lamports.
    pub min_sol_output: u64,
    /// Net lamports received (only for [`OrderFate::Landed`]).
    pub received_lamports: Option<u64>,
    /// Simulated time to reach the chain, in milliseconds.
    pub latency_ms: u64,
    /// The ground truth.
    pub fate: OrderFate,
    /// What the executor reported to the exit engine.
    pub reported: SellOutcome,
}

/// The simulated genesis launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchReport {
    /// Tokens the atomic buy acquired.
    pub position_tokens: u64,
    /// Serialized size of the signed launch transaction.
    pub wire_size_bytes: usize,
    /// Time to assemble and sign it, in microseconds.
    pub assembly_micros: u64,
}

/// Proof that nothing left the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryReport {
    /// Launch transactions assembled and signed (never sent).
    pub launch_transactions_assembled: u64,
    /// Sell transactions assembled and signed.
    pub sell_transactions_assembled: u64,
    /// Bundles the in-memory block engine accepted and discarded.
    pub bundles_swallowed: u64,
    /// Tips committed in those bundles. Virtual: no tip was ever paid.
    pub virtual_tip_lamports: u64,
}

/// The outcome of one simulation.
#[derive(Debug, Clone)]
pub struct SimulationReport {
    /// The scenario seed.
    pub seed: u64,
    /// The genesis launch.
    pub launch: LaunchReport,
    /// The exit engine's final accounting.
    pub exit: ExitSummary,
    /// The stream monitor's final health.
    pub monitor: HealthStatus,
    /// Every order, with ground truth and the reported outcome.
    pub orders: Vec<ExecutedOrder>,
    /// The market's tally.
    pub market: MarketLedger,
    /// Final reserves.
    pub final_curve: CurveParams,
    /// Whether the curve completed.
    pub curve_complete: bool,
    /// Stream updates lost to a full queue.
    pub stream_updates_dropped: u64,
    /// What crossed the process boundary: nothing.
    pub boundary: BoundaryReport,
    /// Pipeline latencies (shared across runs that share the metrics).
    pub metrics: MetricsSnapshot,
}

impl SimulationReport {
    /// Whether the exit engine's books agree with the market's ground truth.
    ///
    /// - every token is in exactly one bucket (sold, in flight, unresolved,
    ///   unsold);
    /// - what the engine counts as sold really left the wallet;
    /// - what really left the wallet is explained by sold tokens plus orders
    ///   whose outcome the engine does not know yet.
    #[must_use]
    pub fn accounting_consistent(&self) -> bool {
        let position = self.exit.position;
        let buckets = [
            position.tokens_sold,
            position.tokens_in_flight,
            position.tokens_unresolved,
            position.tokens_unsold,
        ];
        let total = buckets
            .iter()
            .try_fold(0u64, |sum, part| sum.checked_add(*part));
        let ground_truth = self.market.our_tokens_sold;
        let unconfirmed = position
            .tokens_in_flight
            .saturating_add(position.tokens_unresolved);
        total == Some(self.launch.position_tokens)
            && position.tokens_sold <= ground_truth
            && ground_truth <= position.tokens_sold.saturating_add(unconfirmed)
    }

    /// A hash of everything that is deterministic for a seed (latencies measured
    /// on the wall clock are excluded). Equal seeds give equal fingerprints.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut hash = Fnv::new();
        hash.write(self.seed);
        hash.write(self.launch.position_tokens);
        let position = self.exit.position;
        for value in [
            u64::from(position.orders_issued),
            position.tokens_sold,
            position.tokens_in_flight,
            position.tokens_unresolved,
            position.tokens_unsold,
            u64::from(position.dropped),
            u64::from(position.stop_loss_fired),
            position.committed_cost_lamports,
            self.exit.observations,
            self.exit.decode_errors,
            u64::from(self.exit.interrupted),
            u64::from(self.exit.unsettled),
            self.monitor.reconnects,
            self.market.our_tokens_sold,
            self.market.our_lamports_received,
            u64::from(self.market.external_buys),
            u64::from(self.market.external_sells),
            self.final_curve.virtual_token_reserves,
            self.final_curve.virtual_sol_reserves,
            self.final_curve.real_token_reserves,
            self.stream_updates_dropped,
        ] {
            hash.write(value);
        }
        for order in &self.orders {
            hash.write(u64::from(order.sequence));
            hash.write(order.tokens);
            hash.write(order.min_sol_output);
            hash.write(order.received_lamports.unwrap_or(u64::MAX));
            hash.write(order.latency_ms);
            hash.write(match order.fate {
                OrderFate::Landed => 1,
                OrderFate::SlippageRejected => 2,
                OrderFate::CurveComplete => 3,
                OrderFate::Dropped => 4,
            });
            hash.write(match order.reported {
                SellOutcome::Landed { slot } => slot,
                SellOutcome::Failed => 1,
                SellOutcome::Simulated => 2,
                SellOutcome::Unknown => 3,
            });
        }
        hash.finish()
    }
}

/// FNV-1a over 64-bit words.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xCBF2_9CE4_8422_2325)
    }

    fn write(&mut self, word: u64) {
        for byte in word.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

// --- Shared market ------------------------------------------------------------------------------

struct MarketInner {
    market: MarketState,
    jitter: SplitMix64,
    last_delivery: Instant,
}

/// The market and the path its changes take to the stream consumer.
struct SharedMarket {
    inner: Mutex<MarketInner>,
    curve_account: Pubkey,
    genesis: Instant,
    jitter_ms: (u64, u64),
    queue: mpsc::Sender<(Instant, StreamUpdate)>,
    queue_drops: AtomicU64,
}

impl SharedMarket {
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(
            Instant::now()
                .saturating_duration_since(self.genesis)
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    }

    /// Runs `change` with the market locked. The closure never awaits.
    fn with<R>(
        &self,
        change: impl FnOnce(&mut MarketInner, u64) -> Result<R, SimulationError>,
    ) -> Result<R, SimulationError> {
        let elapsed = self.elapsed_ms();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| SimulationError::StatePoisoned)?;
        change(&mut inner, elapsed)
    }

    /// Queues the current state for delivery after a jittered delay. Deliveries
    /// stay in order: a stream does not reorder.
    fn publish(&self, inner: &mut MarketInner) {
        let update = inner.market.account_update(self.curve_account);
        let jitter = Duration::from_millis(
            inner
                .jitter
                .range_inclusive(self.jitter_ms.0, self.jitter_ms.1),
        );
        let at = (Instant::now() + jitter).max(inner.last_delivery);
        inner.last_delivery = at;
        if self.queue.try_send((at, update)).is_err() {
            self.queue_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Applies the trades in order, each at its time.
async fn drive_market(
    shared: Arc<SharedMarket>,
    trades: Vec<ScheduledTrade>,
    mut shutdown: crate::monitor::ShutdownSignal,
) -> Result<(), SimulationError> {
    for trade in trades {
        let at = shared.genesis + Duration::from_millis(trade.at_ms);
        tokio::select! {
            biased;
            () = shutdown.wait() => return Ok(()),
            () = sleep_until(at) => {}
        }
        shared.with(|inner, elapsed| {
            let changed = match trade.flow {
                TradeFlow::Buy { lamports } => inner.market.external_buy(lamports, elapsed)?,
                TradeFlow::SellHoldings { bps } => inner.market.external_sell(bps, elapsed)?,
            };
            if changed {
                shared.publish(inner);
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Moves queued updates to the stream at their delivery time, and keeps the
/// stream alive with heartbeats while the market is quiet.
async fn deliver_updates(
    mut queue: mpsc::Receiver<(Instant, StreamUpdate)>,
    stream: mpsc::Sender<StreamUpdate>,
    heartbeat: Duration,
    mut shutdown: crate::monitor::ShutdownSignal,
) {
    let mut next_heartbeat = Instant::now() + heartbeat;
    loop {
        tokio::select! {
            biased;
            () = shutdown.wait() => return,
            item = queue.recv() => {
                let Some((at, update)) = item else { return };
                tokio::select! {
                    biased;
                    () = shutdown.wait() => return,
                    () = sleep_until(at) => {}
                }
                // A full stream queue means the consumer lags: the update is lost,
                // exactly as a slow gRPC client loses data.
                let _ = stream.try_send(update);
                next_heartbeat = Instant::now() + heartbeat;
            }
            () = sleep_until(next_heartbeat) => {
                let _ = stream.try_send(StreamUpdate::Heartbeat);
                next_heartbeat = Instant::now() + heartbeat;
            }
        }
    }
}

// --- Simulated Geyser transport -----------------------------------------------------------------

/// A [`StreamTransport`] that serves the simulated market's updates and can
/// suffer one outage: the connection drops at its start, reconnections fail
/// until its end, and the first connection after it opens with the latest
/// account snapshot only (a real endpoint does not replay what was missed).
pub struct SimulatedTransport {
    inbox: Arc<AsyncMutex<mpsc::Receiver<StreamUpdate>>>,
    genesis: Instant,
    outage: Option<Outage>,
    recovered: bool,
}

impl SimulatedTransport {
    fn new(
        inbox: Arc<AsyncMutex<mpsc::Receiver<StreamUpdate>>>,
        genesis: Instant,
        outage: Option<Outage>,
    ) -> Self {
        Self {
            inbox,
            genesis,
            outage,
            recovered: false,
        }
    }
}

/// The live half of [`SimulatedTransport`].
pub struct SimulatedStream {
    inbox: Arc<AsyncMutex<mpsc::Receiver<StreamUpdate>>>,
    preload: Option<StreamUpdate>,
    cut_at: Option<Instant>,
}

impl StreamTransport for SimulatedTransport {
    type Stream = SimulatedStream;

    async fn connect(
        &mut self,
        _request: &SubscriptionRequest,
    ) -> Result<SimulatedStream, MonitorError> {
        let now = Instant::now();
        let mut preload = None;
        let mut cut_at = None;
        if let Some(outage) = self.outage {
            let start = self.genesis + Duration::from_millis(outage.start_ms);
            let end = start + Duration::from_millis(outage.duration_ms);
            if now >= start && now < end {
                return Err(MonitorError::Connection {
                    kind: StreamErrorKind::Unreachable,
                });
            }
            if now < start {
                cut_at = Some(start);
            } else if !self.recovered {
                self.recovered = true;
                // Keep only the freshest account state that piled up.
                let mut inbox = self.inbox.lock().await;
                while let Ok(update) = inbox.try_recv() {
                    if matches!(update, StreamUpdate::Account(_)) {
                        preload = Some(update);
                    }
                }
            }
        }
        Ok(SimulatedStream {
            inbox: Arc::clone(&self.inbox),
            preload,
            cut_at,
        })
    }
}

impl EventStream for SimulatedStream {
    async fn next_update(&mut self) -> Result<Option<StreamUpdate>, MonitorError> {
        if let Some(update) = self.preload.take() {
            return Ok(Some(update));
        }
        let mut inbox = self.inbox.lock().await;
        match self.cut_at {
            Some(cut) => {
                tokio::select! {
                    biased;
                    () = sleep_until(cut) => {
                        self.cut_at = None;
                        Err(MonitorError::Connection { kind: StreamErrorKind::Unreachable })
                    }
                    update = inbox.recv() => Ok(update),
                }
            }
            None => Ok(inbox.recv().await),
        }
    }
}

// --- Simulated Block Engine ---------------------------------------------------------------------

/// A [`SellSubmitter`] that stands in for the Jito Block Engine.
///
/// It assembles and signs every sell exactly as the live submitter does (so the
/// builder, the signing path and the assembly latency are exercised), then hands
/// the transaction to an in-memory sink that drops it. It holds no HTTP client
/// and no socket. What follows is simulated: the delay, whether the bundle is
/// dropped, and the execution against the market at the time it lands.
pub struct SimulatedSubmitter {
    market: Arc<SharedMarket>,
    builder: SellBuilder,
    wallet: Arc<SecureWallet>,
    tip: JitoTip,
    blockhash: Hash,
    stress: StressProfile,
    seed: u64,
    metrics: Arc<PipelineMetrics>,
    executed: Mutex<Vec<ExecutedOrder>>,
    assembled: AtomicU64,
    virtual_tips: AtomicU64,
}

impl SimulatedSubmitter {
    fn record(&self, order: ExecutedOrder) {
        if let Ok(mut executed) = self.executed.lock() {
            executed.push(order);
        }
    }

    fn executed(&self) -> Vec<ExecutedOrder> {
        let mut orders = self
            .executed
            .lock()
            .map(|executed| executed.clone())
            .unwrap_or_default();
        orders.sort_by_key(|order| order.sequence);
        orders
    }
}

impl SellSubmitter for SimulatedSubmitter {
    async fn submit(&self, order: &SellOrder) -> Result<SellOutcome, ExitError> {
        let tip_instruction = self.tip.instruction(&self.builder.payer())?;
        let transaction = self.builder.build_signed(
            &order.plan,
            self.blockhash,
            std::slice::from_ref(&tip_instruction),
            &self.wallet,
        )?;
        self.metrics.mark_assembled(order.sequence);
        self.assembled.fetch_add(1, Ordering::Relaxed);
        self.virtual_tips
            .fetch_add(self.tip.lamports(), Ordering::Relaxed);
        // The in-memory block engine: the signed transaction is neither
        // serialised nor sent. It ends here.
        drop(transaction);

        let mut rng = SplitMix64::new(
            self.seed
                ^ u64::from(order.sequence)
                    .wrapping_add(1)
                    .wrapping_mul(0xD6E8_FEB8_6659_FD93),
        );
        let latency_ms = rng.range_inclusive(
            self.stress.submit_latency_ms.0,
            self.stress.submit_latency_ms.1,
        );
        sleep(Duration::from_millis(latency_ms)).await;

        let fate_roll = rng.below(10_000);
        let drop_limit = u64::from(self.stress.drop_bps);
        let unknown_limit = drop_limit.saturating_add(u64::from(self.stress.unknown_bps));
        let dropped = fate_roll < drop_limit;
        let unknown = !dropped && fate_roll < unknown_limit;
        let reaches_chain = !dropped && (!unknown || rng.chance_bps(5_000));

        let simulation_failed = || ExitError::ArithmeticOverflow {
            context: "simulated market",
        };
        let execution = if reaches_chain {
            Some(
                self.market
                    .with(|inner, elapsed| {
                        let result = inner.market.execute_sell(&order.plan, elapsed)?;
                        if matches!(result, SellExecution::Landed { .. }) {
                            self.market.publish(inner);
                        }
                        Ok(result)
                    })
                    .map_err(|_| simulation_failed())?,
            )
        } else {
            None
        };

        let (fate, received_lamports) = match execution {
            Some(SellExecution::Landed { net_lamports, .. }) => {
                (OrderFate::Landed, Some(net_lamports))
            }
            Some(SellExecution::SlippageExceeded) => (OrderFate::SlippageRejected, None),
            Some(SellExecution::CurveComplete) => (OrderFate::CurveComplete, None),
            None => (OrderFate::Dropped, None),
        };
        let reported = if unknown {
            SellOutcome::Unknown
        } else if let Some(SellExecution::Landed { slot, .. }) = execution {
            SellOutcome::Landed { slot }
        } else {
            SellOutcome::Failed
        };
        self.record(ExecutedOrder {
            sequence: order.sequence,
            tokens: order.plan.token_amount,
            min_sol_output: order.plan.min_sol_output_lamports,
            received_lamports,
            latency_ms,
            fate,
            reported,
        });
        Ok(reported)
    }
}

// --- The harness --------------------------------------------------------------------------------

/// Refuses a live configuration.
///
/// # Errors
/// [`SimulationError::LiveModeRefused`] when `config.dry_run` is `false`.
pub fn ensure_dry_run(config: &Config) -> Result<(), SimulationError> {
    if config.dry_run {
        Ok(())
    } else {
        Err(SimulationError::LiveModeRefused)
    }
}

/// Runs simulated token lifecycles. See the module documentation.
#[derive(Debug)]
pub struct SimulationHarness {
    scenario: ScenarioConfig,
    metrics: Arc<PipelineMetrics>,
}

impl SimulationHarness {
    /// A harness for `scenario`.
    ///
    /// # Errors
    /// [`SimulationError::InvalidScenario`] if the scenario is inconsistent.
    pub fn new(scenario: ScenarioConfig) -> Result<Self, SimulationError> {
        scenario.validate()?;
        Ok(Self {
            scenario,
            metrics: Arc::new(PipelineMetrics::new()),
        })
    }

    /// Like [`SimulationHarness::new`], after checking that `config` is a
    /// dry-run configuration.
    ///
    /// # Errors
    /// [`SimulationError::LiveModeRefused`] for `DRY_RUN=false`, otherwise see
    /// [`SimulationHarness::new`].
    pub fn from_config(config: &Config, scenario: ScenarioConfig) -> Result<Self, SimulationError> {
        ensure_dry_run(config)?;
        Self::new(scenario)
    }

    /// Records into `metrics` instead of a private set, so several runs (or a
    /// run and the live pipeline) can share one set of histograms.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<PipelineMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// The metrics this harness records into.
    #[must_use]
    pub fn metrics(&self) -> &Arc<PipelineMetrics> {
        &self.metrics
    }

    /// The scenario.
    #[must_use]
    pub fn scenario(&self) -> &ScenarioConfig {
        &self.scenario
    }

    /// Simulates the complete lifecycle and reports what happened.
    ///
    /// # Errors
    /// Any [`SimulationError`]: a build failure, a failed task, or an engine
    /// error. Stress (lost bundles, slippage, an outage) is not an error: it is
    /// what the report describes.
    pub async fn run(&self) -> Result<SimulationReport, SimulationError> {
        let scenario = &self.scenario;
        scenario.validate()?;
        let genesis = Instant::now();
        let seed = scenario.seed;

        // --- identities: deterministic, public, unfunded ---
        let wallet_seed = Zeroizing::new(derive_seed_bytes(seed, 1));
        let wallet_bytes = Zeroizing::new(Keypair::new_from_array(*wallet_seed).to_bytes());
        let wallet = Arc::new(SecureWallet::from_keypair_bytes(wallet_bytes.as_slice())?);
        let mint = MintSigner::from_seed(derive_seed_bytes(seed, 2));
        let blockhash = Hash::new_from_array(derive_seed_bytes(seed, 3));

        // --- genesis launch: Create + atomic Buy, assembled and signed, never sent ---
        let launch_curve = CurveParams::default();
        let buy = BuyPlan::from_supply_bps(
            &launch_curve,
            scenario.launch_supply_bps,
            scenario.fee_bps,
            scenario.entry_slippage_bps,
        )?;
        let metadata = LaunchMetadata::new(
            "Krypton Simulation",
            "KSIM",
            &format!("https://ipfs.io/ipfs/{}", "b".repeat(46)),
        )?;
        let params = LaunchParams {
            metadata,
            variant: CreateVariant::CreateV2,
            options: CreateOptions::default(),
            creator: None,
            buy,
            compute_unit_limit: 300_000,
            priority_fee_micro_lamports: 100_000,
            max_priority_fee_lamports: 5_000_000,
            buyback_fee_recipient: None,
        };
        let launch_builder = LaunchBuilder::new(wallet.pubkey(), mint.pubkey(), params)?;
        let launch_watch = Stopwatch::start();
        let required = launch_builder
            .params()
            .required_balance(SIMULATED_CREATE_RENT_LAMPORTS, 0)?;
        launch_builder.ensure_funded(&FundingSnapshot {
            available_lamports: required.saturating_add(SIMULATED_FUNDING_HEADROOM_LAMPORTS),
            create_rent_lamports: SIMULATED_CREATE_RENT_LAMPORTS,
            tip_lamports: 0,
        })?;
        let compiled = launch_builder.compile(blockhash, &[], &[])?;
        let wire_size_bytes = compiled.wire_size;
        let launch_transaction = launch_builder.sign(compiled.message, &wallet, &mint)?;
        drop(launch_transaction);
        let launch = LaunchReport {
            position_tokens: buy.token_amount,
            wire_size_bytes,
            assembly_micros: launch_watch.elapsed_micros(),
        };
        if wire_size_bytes > MAX_TRANSACTION_SIZE {
            return Err(SimulationError::InvalidScenario {
                reason: "the launch transaction does not fit a packet",
            });
        }
        let curve_account = launch_builder.addresses().bonding_curve;

        // --- the market after our buy ---
        let mut state = MarketState::new(launch_curve, scenario.fee_bps);
        state.apply_creator_buy(&buy)?;
        let (queue_tx, queue_rx) = mpsc::channel(QUEUE_CAPACITY);
        let shared = Arc::new(SharedMarket {
            inner: Mutex::new(MarketInner {
                market: state,
                jitter: SplitMix64::new(seed ^ 0x57EA_57EA_57EA_57EA),
                last_delivery: genesis,
            }),
            curve_account,
            genesis,
            jitter_ms: scenario.stress.stream_jitter_ms,
            queue: queue_tx,
            queue_drops: AtomicU64::new(0),
        });
        shared.with(|inner, _| {
            shared.publish(inner);
            Ok(())
        })?;

        let mut trades = generate_flow(seed, &scenario.stress.flow, 0, scenario.horizon_ms);
        trades.extend(scenario.stress.shocks.iter().copied());
        trades.sort_by_key(|trade| trade.at_ms);

        // --- the exit ---
        let sell_builder = SellBuilder::new(
            wallet.pubkey(),
            mint.pubkey(),
            CreateVariant::CreateV2,
            SellSettings {
                compute_unit_limit: 200_000,
                priority_fee_micro_lamports: 100_000,
                max_priority_fee_lamports: 5_000_000,
                buyback_fee_recipient: None,
            },
        )?;
        let tip = JitoTip::new(
            JITO_TIP_ACCOUNTS
                .get(usize::try_from(seed % 8).unwrap_or(0))
                .copied()
                .unwrap_or(FALLBACK_TIP_ACCOUNT),
            scenario.tip_lamports,
            scenario.tip_lamports,
        )?;
        let order_cost = sell_builder.order_cost_lamports(tip.lamports())?;
        let budget = order_cost
            .checked_mul(u64::from(scenario.max_exit_orders))
            .ok_or(SimulationError::InvalidScenario {
                reason: "exit budget overflows",
            })?;
        let engine = ExitEngine::new(
            ExitPlan::evenly_spaced(
                scenario.exit_tranches,
                scenario.exit_total_bps,
                scenario.stop_loss_bps,
            )?,
            ExitSettings {
                fee_bps: scenario.fee_bps,
                slippage_bps: scenario.exit_slippage_bps,
                order_cost_lamports: order_cost,
                max_total_cost_lamports: budget,
            },
            Position {
                tokens: buy.token_amount,
                entry: PriceReference::after_buy(&launch_curve, &buy)?,
            },
        )?;
        let manager = ExitManager::new(engine, curve_account, genesis)
            .with_metrics(Arc::clone(&self.metrics));
        let submitter = Arc::new(SimulatedSubmitter {
            market: Arc::clone(&shared),
            builder: sell_builder,
            wallet: Arc::clone(&wallet),
            tip,
            blockhash,
            stress: scenario.stress.clone(),
            seed,
            metrics: Arc::clone(&self.metrics),
            executed: Mutex::new(Vec::new()),
            assembled: AtomicU64::new(0),
            virtual_tips: AtomicU64::new(0),
        });

        // --- wiring ---
        let (stream_tx, stream_rx) = mpsc::channel(QUEUE_CAPACITY);
        let transport = SimulatedTransport::new(
            Arc::new(AsyncMutex::new(stream_rx)),
            genesis,
            scenario.stress.outage,
        );
        let request = SubscriptionRequest {
            accounts: vec![curve_account],
            instruction_program: Some(PUMP_PROGRAM_ID),
            commitment: StreamCommitment::Processed,
        };
        let monitor = GeyserMonitor::new(transport, request, MonitorSettings::default())?
            .with_jitter_seed(seed | 1);
        let (events_tx, events_rx) = mpsc::channel(QUEUE_CAPACITY);
        let (health_tx, _health_rx) = watch::channel(HealthStatus::default());
        let (orders_tx, orders_rx) = order_channel();
        let (reports_tx, reports_rx) = report_channel();
        let (trigger, signal) = shutdown_channel();

        let monitor_task = tokio::spawn(monitor.run(events_tx, health_tx, signal.clone()));
        let delivery_task = tokio::spawn(deliver_updates(
            queue_rx,
            stream_tx,
            Duration::from_millis(scenario.heartbeat_ms),
            signal.clone(),
        ));
        let driver_task = tokio::spawn(drive_market(Arc::clone(&shared), trades, signal.clone()));
        let executor_task = tokio::spawn(run_sell_executor(
            Arc::clone(&submitter),
            orders_rx,
            reports_tx,
            signal.clone(),
            Duration::from_secs(5),
        ));

        let exit = manager
            .run(events_rx, orders_tx, reports_rx, signal.clone())
            .await;

        // Whatever happened, stop every task before looking at the results.
        trigger.trigger();
        let monitor_result = monitor_task
            .await
            .map_err(|_| SimulationError::TaskFailed { task: "monitor" })?;
        delivery_task
            .await
            .map_err(|_| SimulationError::TaskFailed { task: "delivery" })?;
        let driver_result = driver_task
            .await
            .map_err(|_| SimulationError::TaskFailed { task: "market" })?;
        executor_task
            .await
            .map_err(|_| SimulationError::TaskFailed { task: "executor" })?;
        let exit = exit?;
        driver_result?;
        let monitor_health = monitor_result?;

        // --- results ---
        let (market_ledger, final_curve, curve_complete) = shared.with(|inner, _| {
            Ok((
                inner.market.ledger(),
                inner.market.curve(),
                inner.market.complete(),
            ))
        })?;
        let report = SimulationReport {
            seed,
            launch,
            exit,
            monitor: monitor_health,
            orders: submitter.executed(),
            market: market_ledger,
            final_curve,
            curve_complete,
            stream_updates_dropped: shared.queue_drops.load(Ordering::Relaxed),
            boundary: BoundaryReport {
                launch_transactions_assembled: 1,
                sell_transactions_assembled: submitter.assembled.load(Ordering::Relaxed),
                bundles_swallowed: submitter.assembled.load(Ordering::Relaxed),
                virtual_tip_lamports: submitter.virtual_tips.load(Ordering::Relaxed),
            },
            metrics: self.metrics.snapshot(),
        };
        // Counts and flags only: no keys, signatures, amounts or balances.
        info!(
            orders = report.exit.position.orders_issued,
            landed = report.market.our_sells_landed,
            rejected = report.market.our_sells_rejected,
            reconnects = report.monitor.reconnects,
            stop_loss = report.exit.position.stop_loss_fired,
            consistent = report.accounting_consistent(),
            "simulation finished"
        );
        self.metrics.log_summary();
        Ok(report)
    }
}
