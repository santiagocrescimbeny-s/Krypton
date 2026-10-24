//! Live execution: the verified builders wired to the Jito Block Engine, behind
//! the dry-to-live barrier and the spending guards (Phase 7).
//!
//! ```text
//!  Config ──arm()──▶ LiveExecutor ──launch()──▶ guards.reserve ─▶ build+sign ─▶ sendBundle ─▶ poll
//!                        │                                              (tip from TipBook)
//!                        ├──sell_submitter()──▶ LiveSellSubmitter (one per exit; same guards)
//!                        └──run_exit()────────▶ GeyserMonitor ─▶ ExitManager ─▶ executor
//! ```
//!
//! # Fail-safe
//! A [`LiveExecutor`] only exists after [`gate::arm`] succeeded (explicit
//! flags, confirmation phrase, capital limits, a keypair file that is the
//! configured wallet) and after checking that the engine client is not in
//! dry-run mode. The engine client keeps its own dry-run switch as a second,
//! independent barrier.
//!
//! # Money
//! Every order reserves its worst-case cost first ([`SpendingGuards`]) and
//! settles afterwards: *committed* when it landed or may have landed, *released*
//! when it definitely did not. Tips come from the [`TipBook`] and are capped by
//! the policy; a launch that fails the guards is never even built.
//!
//! # Logging
//! Counts, sequences and static labels only: no amounts, signatures, balances or
//! keys.
//!
//! # Not included
//! The gRPC transport for Yellowstone (`tonic`/`prost`) cannot be added without
//! regenerating `Cargo.lock`; [`LiveExecutor::run_exit`] takes any
//! [`StreamTransport`], so the adapter specified in `docs/yellowstone-adapter.md`
//! plugs in without touching this module.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::transaction::VersionedTransaction;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};

use super::gate::{self, ArmedLive};
use super::guards::{Reservation, SpendKind, SpendingGuards};
use super::tip::{TipBasis, TipBook, TipQuote, Urgency};
use crate::builder::alt::LaunchLookupTable;
use crate::builder::constants::PUMP_PROGRAM_ID;
use crate::builder::sell::SellBuilder;
use crate::builder::transaction::{FundingSnapshot, LaunchBuilder, MintSigner};
use crate::config::Config;
use crate::errors::{ExitError, JitoError, LiveError};
use crate::metrics::PipelineMetrics;
use crate::monitor::exit::{
    order_channel, report_channel, run_sell_executor, BalanceReader, ExitEngine, ExitManager,
    ExitSummary, SellOrder, SellOutcome, SellSubmitter,
};
use crate::monitor::geyser::{
    GeyserMonitor, HealthStatus, MonitorSettings, StreamCommitment, StreamTransport,
    SubscriptionRequest,
};
use crate::monitor::submit::{is_definite_failure, TimedBlockhash, MAX_BLOCKHASH_AGE};
use crate::monitor::{shutdown_channel, ShutdownSignal};
use crate::network::{BundleId, InflightStatus, JitoClient, JitoTip, JITO_TIP_ACCOUNTS};
use crate::security::SecureWallet;

/// Default pause between two bundle status checks.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Default number of status checks before an outcome is declared unknown.
pub const DEFAULT_MAX_POLLS: u32 = 12;
/// How long submissions in progress may keep running after shutdown.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

// --- The Block Engine seam -------------------------------------------------------------

/// What live execution needs from the Block Engine. [`JitoClient`] implements it;
/// tests and rehearsals implement it with scripted answers.
pub trait BlockEngine: Send + Sync + 'static {
    /// `true` when the client never submits (see `DRY_RUN`).
    fn is_dry_run(&self) -> bool;

    /// Submits a bundle. Never retried by the executor.
    fn send_bundle(
        &self,
        transactions: &[VersionedTransaction],
    ) -> impl Future<Output = Result<BundleId, JitoError>> + Send;

    /// The status of a submitted bundle.
    fn inflight_status(
        &self,
        id: &BundleId,
    ) -> impl Future<Output = Result<InflightStatus, JitoError>> + Send;
}

impl BlockEngine for JitoClient {
    fn is_dry_run(&self) -> bool {
        JitoClient::is_dry_run(self)
    }

    async fn send_bundle(
        &self,
        transactions: &[VersionedTransaction],
    ) -> Result<BundleId, JitoError> {
        JitoClient::send_bundle(self, transactions).await
    }

    async fn inflight_status(&self, id: &BundleId) -> Result<InflightStatus, JitoError> {
        JitoClient::inflight_status(self, id).await
    }
}

/// How a submitted bundle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleOutcome {
    /// Landed in `slot`.
    Landed {
        /// Landing slot.
        slot: u64,
    },
    /// Definitely did not land.
    Failed,
    /// It may still land, or have landed: the money counts as spent.
    Unknown,
}

/// Polls a bundle: the first check is immediate, later ones are `interval` apart.
async fn poll_bundle<E: BlockEngine>(
    engine: &E,
    id: &BundleId,
    interval: Duration,
    max_polls: u32,
) -> BundleOutcome {
    for attempt in 0..max_polls {
        if attempt > 0 {
            sleep(interval).await;
        }
        match engine.inflight_status(id).await {
            Ok(InflightStatus::Landed { slot }) => return BundleOutcome::Landed { slot },
            Ok(status) if is_definite_failure(&status, attempt) => return BundleOutcome::Failed,
            // Pending, not found, or an `Invalid` still within its soft retries.
            Ok(_) => {}
            Err(error) if error.is_retryable() => {
                debug!(%error, "bundle status check failed; retrying");
            }
            Err(error) => {
                // The bundle was accepted, so this is not a failure.
                warn!(%error, "bundle status check failed permanently");
                return BundleOutcome::Unknown;
            }
        }
    }
    BundleOutcome::Unknown
}

// --- Clock ---------------------------------------------------------------------------------

/// Source of Unix seconds, replaceable in tests.
pub type Clock = Arc<dyn Fn() -> Result<u64, LiveError> + Send + Sync>;

/// Seconds since the Unix epoch.
///
/// # Errors
/// [`LiveError::ClockUnavailable`] if the system clock is before the epoch.
pub fn unix_now() -> Result<u64, LiveError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| LiveError::ClockUnavailable)
}

fn pick_tip_account(accounts: &[Pubkey], salt: u8) -> Result<Pubkey, LiveError> {
    let count = accounts.len();
    let index = usize::from(salt).checked_rem(count).unwrap_or(0);
    accounts
        .get(index)
        .copied()
        .ok_or(LiveError::InvalidTipPolicy {
            reason: "no tip accounts configured",
        })
}

// --- Launch ---------------------------------------------------------------------------------

/// Everything a launch needs besides the executor's own state.
#[derive(Debug, Clone, Copy)]
pub struct LaunchRequest<'a> {
    /// The launch to send.
    pub builder: &'a LaunchBuilder,
    /// The mint's signer.
    pub mint: &'a MintSigner,
    /// The launch's lookup table.
    pub table: &'a LaunchLookupTable,
    /// Current lamport balance of the payer (`getBalance`).
    pub available_lamports: u64,
    /// Rent of the accounts `Create` allocates (measure it by simulation).
    pub create_rent_lamports: u64,
    /// Freshly fetched blockhash: the last datum injected before signing.
    pub recent_blockhash: Hash,
}

/// What a launch attempt produced.
#[derive(Debug, Clone)]
pub struct LaunchReceipt {
    /// How the bundle ended. [`BundleOutcome::Unknown`] means it may have landed.
    pub outcome: BundleOutcome,
    /// The bundle id, when the engine returned one.
    pub bundle: Option<BundleId>,
    /// The tip the bundle carried, in lamports.
    pub tip_lamports: u64,
    /// The rule that chose the tip.
    pub tip_basis: TipBasis,
}

// --- Executor --------------------------------------------------------------------------------

/// Live transaction dispatcher. See the module documentation.
pub struct LiveExecutor<E: BlockEngine> {
    engine: Arc<E>,
    wallet: Arc<SecureWallet>,
    guards: Arc<SpendingGuards>,
    tips: Arc<TipBook>,
    tip_accounts: Vec<Pubkey>,
    poll_interval: Duration,
    max_polls: u32,
    metrics: Option<Arc<PipelineMetrics>>,
    clock: Clock,
}

impl<E: BlockEngine> std::fmt::Debug for LiveExecutor<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveExecutor")
            .field("poll_interval", &self.poll_interval)
            .field("max_polls", &self.max_polls)
            .finish_non_exhaustive()
    }
}

impl LiveExecutor<JitoClient> {
    /// Crosses the dry-to-live barrier for a real configuration and connects to
    /// the Block Engine in `JITO_ENGINE_URL`.
    ///
    /// # Errors
    /// The first failed check of [`gate::arm`], or the client could not be built.
    pub fn arm(config: &Config) -> Result<Self, LiveError> {
        let (wallet, armed) = gate::arm(
            config.dry_run,
            &config.live,
            config.wallet(),
            config.slippage_bps,
            config.jito_tip_lamports,
            config.max_jito_tip_lamports,
        )?;
        // Audit #35: no trusted spending state, no live trading. Before any
        // client exists, so a refusal costs no connection.
        let guards = gate::open_guards(&config.live, armed.limits, unix_now()?)?;
        let client = JitoClient::from_config(config)?;
        Ok(
            Self::with_engine(Arc::new(client), Arc::new(wallet), &armed)?
                .with_guards(Arc::new(guards)),
        )
    }
}

impl<E: BlockEngine> LiveExecutor<E> {
    /// Builds an executor around any [`BlockEngine`] from already-checked
    /// parameters (see [`gate::arm`]).
    ///
    /// # Errors
    /// [`LiveError::EngineIsDryRun`] when the engine client does not submit.
    pub fn with_engine(
        engine: Arc<E>,
        wallet: Arc<SecureWallet>,
        armed: &ArmedLive,
    ) -> Result<Self, LiveError> {
        if engine.is_dry_run() {
            return Err(LiveError::EngineIsDryRun);
        }
        Ok(Self {
            engine,
            wallet,
            guards: Arc::new(SpendingGuards::new(armed.limits)),
            tips: Arc::new(TipBook::new(armed.tip_policy)),
            tip_accounts: JITO_TIP_ACCOUNTS.to_vec(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_polls: DEFAULT_MAX_POLLS,
            metrics: None,
            clock: Arc::new(unix_now),
        })
    }

    /// Overrides the status polling cadence.
    #[must_use]
    pub fn with_polling(mut self, poll_interval: Duration, max_polls: u32) -> Self {
        self.poll_interval = poll_interval;
        self.max_polls = max_polls;
        self
    }

    /// Records pipeline latencies (stream update received to transaction assembled).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<PipelineMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Replaces the tip accounts (for example with the engine's `getTipAccounts`).
    ///
    /// # Errors
    /// [`LiveError::InvalidTipPolicy`] for an empty list.
    pub fn with_tip_accounts(mut self, accounts: Vec<Pubkey>) -> Result<Self, LiveError> {
        if accounts.is_empty() {
            return Err(LiveError::InvalidTipPolicy {
                reason: "no tip accounts configured",
            });
        }
        self.tip_accounts = accounts;
        Ok(self)
    }

    /// Replaces the clock (tests).
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The spending guards.
    #[must_use]
    pub fn guards(&self) -> &Arc<SpendingGuards> {
        &self.guards
    }

    /// Replaces the guards (for example with ones restored from disk).
    #[must_use]
    pub fn with_guards(mut self, guards: Arc<SpendingGuards>) -> Self {
        self.guards = guards;
        self
    }

    /// The tip book: publish tip floors into it.
    #[must_use]
    pub fn tips(&self) -> &Arc<TipBook> {
        &self.tips
    }

    fn settle(
        &self,
        reservation: Reservation,
        outcome: BundleOutcome,
        now_secs: u64,
    ) -> Result<(), LiveError> {
        match outcome {
            BundleOutcome::Landed { .. } | BundleOutcome::Unknown => {
                self.guards.commit(reservation, now_secs)?;
            }
            BundleOutcome::Failed => self.guards.release(reservation)?,
        }
        Ok(())
    }

    /// Sends the launch bundle: guards, tip, assembly, submission, polling.
    ///
    /// Nothing is built or signed unless the guards accept the launch's
    /// worst-case outlay (the buy's `max_sol_cost`, rents, fees and the tip), and
    /// the balance check of the builder passes.
    ///
    /// # Errors
    /// [`LiveError::Guard`] (slippage, cap, drawdown, halt), builder and Block
    /// Engine errors. An ambiguous `sendBundle` failure is **not** an error: it is
    /// reported as [`BundleOutcome::Unknown`] with the money committed.
    pub async fn launch(&self, request: &LaunchRequest<'_>) -> Result<LaunchReceipt, LiveError> {
        let params = request.builder.params();
        self.guards.check_slippage(params.buy.slippage_bps)?;

        let quote: TipQuote = self
            .tips
            .quote(Urgency::Launch, params.buy.max_sol_cost_lamports)?;
        let mint = request.mint.pubkey();
        let salt = mint.to_bytes().first().copied().unwrap_or(0);
        let tip = JitoTip::new(
            pick_tip_account(&self.tip_accounts, salt)?,
            quote.lamports,
            self.tips.policy().cap_lamports(),
        )?;

        let outlay = params
            .required_balance(request.create_rent_lamports, tip.lamports())
            .map_err(LiveError::Builder)?;
        let now = (self.clock)()?;
        let reservation = self.guards.reserve(SpendKind::Entry, outlay, now)?;

        let transaction = match self.assemble_launch(request, &tip) {
            Ok(transaction) => transaction,
            Err(error) => {
                self.guards.release(reservation)?;
                return Err(error);
            }
        };

        let bundle = match self
            .engine
            .send_bundle(std::slice::from_ref(&transaction))
            .await
        {
            Ok(id) => id,
            Err(error) if error.may_have_been_accepted() => {
                warn!("launch sendBundle outcome ambiguous; money stays committed");
                self.settle(reservation, BundleOutcome::Unknown, now)?;
                return Ok(LaunchReceipt {
                    outcome: BundleOutcome::Unknown,
                    bundle: None,
                    tip_lamports: tip.lamports(),
                    tip_basis: quote.basis,
                });
            }
            Err(error) => {
                self.guards.release(reservation)?;
                return Err(error.into());
            }
        };
        info!("launch bundle submitted");

        let outcome = poll_bundle(&*self.engine, &bundle, self.poll_interval, self.max_polls).await;
        self.settle(reservation, outcome, (self.clock)()?)?;
        Ok(LaunchReceipt {
            outcome,
            bundle: Some(bundle),
            tip_lamports: tip.lamports(),
            tip_basis: quote.basis,
        })
    }

    fn assemble_launch(
        &self,
        request: &LaunchRequest<'_>,
        tip: &JitoTip,
    ) -> Result<VersionedTransaction, LiveError> {
        let tip_instruction = tip.instruction(&request.builder.payer())?;
        let funding = FundingSnapshot {
            available_lamports: request.available_lamports,
            create_rent_lamports: request.create_rent_lamports,
            tip_lamports: tip.lamports(),
        };
        let tables = [request.table.to_account()];
        Ok(request.builder.build_signed_checked(
            &funding,
            request.recent_blockhash,
            &tables,
            std::slice::from_ref(&tip_instruction),
            &self.wallet,
            request.mint,
        )?)
    }

    /// The order submitter for the exit of one coin. It shares this executor's
    /// guards, tips, engine and clock.
    #[must_use]
    pub fn sell_submitter(
        &self,
        builder: SellBuilder,
        blockhash: watch::Receiver<TimedBlockhash>,
    ) -> LiveSellSubmitter<E> {
        LiveSellSubmitter {
            engine: Arc::clone(&self.engine),
            wallet: Arc::clone(&self.wallet),
            guards: Arc::clone(&self.guards),
            tips: Arc::clone(&self.tips),
            tip_accounts: self.tip_accounts.clone(),
            builder,
            blockhash,
            poll_interval: self.poll_interval,
            max_polls: self.max_polls,
            metrics: self.metrics.clone(),
            clock: Arc::clone(&self.clock),
        }
    }

    /// Runs the live exit: the stream supervisor, the exit manager and the order
    /// executor, until the exit is done or `shutdown` fires.
    ///
    /// `transport` is the Geyser/Yellowstone connection (any
    /// [`StreamTransport`]); the subscription watches `curve_account` and the
    /// Pump program's instructions. Metrics, when set, time every curve update
    /// from receipt to the assembled sell.
    ///
    /// # Errors
    /// [`LiveError::Exit`] / [`LiveError::Monitor`] from the pipeline, or
    /// [`LiveError::TaskFailed`] if a task died.
    pub async fn run_exit<T: StreamTransport + Sync + 'static>(
        &self,
        transport: T,
        exit: ExitRun,
        shutdown: ShutdownSignal,
    ) -> Result<ExitReport, LiveError> {
        let submitter = Arc::new(self.sell_submitter(exit.builder, exit.blockhash));
        let pipeline = ExitPipeline {
            engine: exit.engine,
            curve_account: exit.curve_account,
            genesis: exit.genesis,
            monitor: exit.monitor,
            drain_timeout: exit.drain_timeout,
            balance: exit.balance,
        };
        run_exit_pipeline(
            transport,
            submitter,
            pipeline,
            self.metrics.clone(),
            shutdown,
        )
        .await
    }
}

/// The parts of an exit that do not depend on how orders are submitted.
pub struct ExitPipeline {
    /// The exit engine (plan, settings, position).
    pub engine: ExitEngine,
    /// The coin's bonding curve account.
    pub curve_account: Pubkey,
    /// The instant the launch was submitted: the zero of the 1 s to 10 s window.
    pub genesis: Instant,
    /// Stream supervisor settings.
    pub monitor: MonitorSettings,
    /// How long submissions in progress may run after shutdown.
    pub drain_timeout: Duration,
    /// Reads the position's token balance to settle unknown orders at the end
    /// (audit finding #45); `None` leaves them unresolved.
    pub balance: Option<BalanceReader>,
}

/// Runs the exit pipeline around any [`SellSubmitter`]: the stream supervisor,
/// the exit manager and the order executor, until the exit is done or
/// `shutdown` fires.
///
/// [`LiveExecutor::run_exit`] is this function with the live submitter; the
/// dry-run entry point passes a submitter that never leaves the process.
///
/// # Errors
/// [`LiveError::Exit`] / [`LiveError::Monitor`] from the pipeline, or
/// [`LiveError::TaskFailed`] if a task died.
pub async fn run_exit_pipeline<T, S>(
    transport: T,
    submitter: Arc<S>,
    pipeline: ExitPipeline,
    metrics: Option<Arc<PipelineMetrics>>,
    shutdown: ShutdownSignal,
) -> Result<ExitReport, LiveError>
where
    T: StreamTransport + Sync + 'static,
    S: SellSubmitter + 'static,
{
    let request = subscription_for(pipeline.curve_account);
    let monitor = GeyserMonitor::new(transport, request, pipeline.monitor)?;

    let mut manager = ExitManager::new(pipeline.engine, pipeline.curve_account, pipeline.genesis);
    if let Some(metrics) = metrics {
        manager = manager.with_metrics(metrics);
    }
    if let Some(reader) = pipeline.balance {
        manager = manager.with_balance_reader(reader);
    }

    let (events_tx, events_rx) = mpsc::channel(1_024);
    let (health_tx, _health_rx) = watch::channel(HealthStatus::default());
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, reports_rx) = report_channel();
    let (trigger, internal) = shutdown_channel();
    let trigger = Arc::new(trigger);

    // An external shutdown stops every task; so does the manager finishing.
    let forward = {
        let trigger = Arc::clone(&trigger);
        let mut external = shutdown;
        tokio::spawn(async move {
            external.wait().await;
            trigger.trigger();
        })
    };

    let monitor_task = tokio::spawn(monitor.run(events_tx, health_tx, internal.clone()));
    let executor_task = tokio::spawn(run_sell_executor(
        submitter,
        orders_rx,
        reports_tx,
        internal.clone(),
        pipeline.drain_timeout,
    ));

    let summary = manager
        .run(events_rx, orders_tx, reports_rx, internal)
        .await;

    trigger.trigger();
    forward.abort();
    let health = monitor_task
        .await
        .map_err(|_| LiveError::TaskFailed { task: "monitor" })?;
    executor_task
        .await
        .map_err(|_| LiveError::TaskFailed { task: "executor" })?;
    Ok(ExitReport {
        summary: summary?,
        health: health?,
    })
}

/// The subscription of a live exit: the coin's bonding curve, plus the Pump
/// program's instructions that touch it, at `processed` commitment.
#[must_use]
pub fn subscription_for(curve_account: Pubkey) -> SubscriptionRequest {
    SubscriptionRequest {
        accounts: vec![curve_account],
        instruction_program: Some(PUMP_PROGRAM_ID),
        commitment: StreamCommitment::Processed,
    }
}

/// Inputs of [`LiveExecutor::run_exit`].
pub struct ExitRun {
    /// The exit engine (plan, settings, position).
    pub engine: ExitEngine,
    /// The coin's bonding curve account.
    pub curve_account: Pubkey,
    /// The instant the launch was submitted: the zero of the 1 s to 10 s window.
    pub genesis: Instant,
    /// Builds the sell transactions.
    pub builder: SellBuilder,
    /// A blockhash kept fresh by the caller.
    pub blockhash: watch::Receiver<TimedBlockhash>,
    /// Stream supervisor settings.
    pub monitor: MonitorSettings,
    /// How long submissions in progress may run after shutdown.
    pub drain_timeout: Duration,
    /// Reads the position's token balance to settle unknown orders at the end
    /// (audit finding #45); `None` leaves them unresolved.
    pub balance: Option<BalanceReader>,
}

/// What a live exit produced.
#[derive(Debug, Clone, Copy)]
pub struct ExitReport {
    /// The exit engine's final accounting.
    pub summary: ExitSummary,
    /// The stream supervisor's final health.
    pub health: HealthStatus,
}

// --- Sell submitter ------------------------------------------------------------------------

/// [`SellSubmitter`] for live exits: dynamic tip, spending guards, real Block
/// Engine. Built by [`LiveExecutor::sell_submitter`].
pub struct LiveSellSubmitter<E: BlockEngine> {
    engine: Arc<E>,
    wallet: Arc<SecureWallet>,
    guards: Arc<SpendingGuards>,
    tips: Arc<TipBook>,
    tip_accounts: Vec<Pubkey>,
    builder: SellBuilder,
    blockhash: watch::Receiver<TimedBlockhash>,
    poll_interval: Duration,
    max_polls: u32,
    metrics: Option<Arc<PipelineMetrics>>,
    clock: Clock,
}

impl<E: BlockEngine> LiveSellSubmitter<E> {
    fn fresh_blockhash(&self) -> Result<Hash, ExitError> {
        let (hash, observed_at) = *self.blockhash.borrow();
        if observed_at.elapsed() > MAX_BLOCKHASH_AGE {
            return Err(ExitError::StaleBlockhash);
        }
        Ok(hash)
    }

    fn now(&self) -> Result<u64, ExitError> {
        (self.clock)().map_err(|_| ExitError::InvalidConfig {
            reason: "the system clock is unavailable",
        })
    }
}

impl<E: BlockEngine> SellSubmitter for LiveSellSubmitter<E> {
    async fn submit(&self, order: &SellOrder) -> Result<SellOutcome, ExitError> {
        // Refuse before anything is reserved, built or signed.
        let blockhash = self.fresh_blockhash()?;
        let quote = self
            .tips
            .quote(Urgency::Exit, order.plan.gross_proceeds_lamports)
            .map_err(|_| ExitError::InvalidConfig {
                reason: "the tip book is unavailable",
            })?;
        let salt = order.sequence;
        let account =
            pick_tip_account(&self.tip_accounts, salt).map_err(|_| ExitError::InvalidConfig {
                reason: "no tip accounts configured",
            })?;
        let tip = JitoTip::new(account, quote.lamports, self.tips.policy().cap_lamports())?;

        let cost = self.builder.order_cost_lamports(tip.lamports())?;
        let now = self.now()?;
        let reservation = self.guards.reserve(SpendKind::Exit, cost, now)?;

        let built = tip
            .instruction(&self.builder.payer())
            .map_err(ExitError::from)
            .and_then(|instruction| {
                self.builder
                    .build_signed(
                        &order.plan,
                        blockhash,
                        std::slice::from_ref(&instruction),
                        &self.wallet,
                    )
                    .map_err(ExitError::from)
            });
        let transaction = match built {
            Ok(transaction) => transaction,
            Err(error) => {
                self.guards.release(reservation)?;
                return Err(error);
            }
        };
        if let Some(metrics) = &self.metrics {
            metrics.mark_assembled(order.sequence);
        }

        let bundle = match self
            .engine
            .send_bundle(std::slice::from_ref(&transaction))
            .await
        {
            Ok(id) => id,
            Err(error) if error.may_have_been_accepted() => {
                warn!(sequence = order.sequence, "sendBundle outcome ambiguous");
                self.guards.commit(reservation, now)?;
                return Ok(SellOutcome::Unknown);
            }
            Err(error) => {
                self.guards.release(reservation)?;
                return Err(error.into());
            }
        };
        debug!(sequence = order.sequence, "sell bundle submitted");

        let outcome = poll_bundle(&*self.engine, &bundle, self.poll_interval, self.max_polls).await;
        let settled_at = self.now().unwrap_or(now);
        match outcome {
            BundleOutcome::Landed { slot } => {
                self.guards.commit(reservation, settled_at)?;
                self.guards
                    .record_return(order.plan.net_proceeds_lamports, settled_at)?;
                Ok(SellOutcome::Landed { slot })
            }
            BundleOutcome::Failed => {
                self.guards.release(reservation)?;
                Ok(SellOutcome::Failed)
            }
            BundleOutcome::Unknown => {
                self.guards.commit(reservation, settled_at)?;
                Ok(SellOutcome::Unknown)
            }
        }
    }
}
